/**
 * Pure rendering for the session-insights View — DOM only, no host/protocol.
 *
 * Kept separate from main.ts (the ext-apps host wiring) so the card can be
 * rendered with sample data in isolation (visual harness) and reasoned about
 * without the postMessage lifecycle.
 */

export type Status = "ready" | "analyzing" | "not_ingested";

export interface Tokens {
  input: number;
  output: number;
  cache_read: number;
  cache_creation: number;
  reasoning: number;
  total: number;
}

export interface TaxNode {
  id: string;
  name: string;
  path?: string;
  root_key?: string;
  color?: string | null;
  letter?: string | null;
  emoji?: string | null;
}

export interface Insights {
  status: Status;
  segments_pending?: number;
  session_ids?: string[];
  missing_session_ids?: string[];
  started_at?: string | null;
  ended_at?: string | null;
  segment_count?: number;
  tokens?: Partial<Tokens>;
  cost_usd?: number;
  taxonomy_nodes?: TaxNode[];
}

// ─── tiny DOM helper ──────────────────────────────────────────────

type Attrs = Record<string, string | number | boolean | undefined>;
function el(
  tag: string,
  attrs: Attrs = {},
  children: (Node | string)[] = [],
): HTMLElement {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === false) continue;
    if (k === "class") node.className = String(v);
    else if (k === "text") node.textContent = String(v);
    else node.setAttribute(k, String(v));
  }
  for (const c of children) node.append(c);
  return node;
}

// ─── formatting ───────────────────────────────────────────────────

function fmtNum(n: number): string {
  if (!Number.isFinite(n) || n <= 0) return "0";
  if (n < 1000) return String(n);
  if (n < 1e6) return `${+(n / 1e3).toFixed(n < 1e4 ? 1 : 0)}k`;
  return `${+(n / 1e6).toFixed(2)}M`;
}

function fmtUsd(n: number): string {
  if (!Number.isFinite(n) || n <= 0) return "$0";
  if (n >= 1) return `$${n.toFixed(2)}`;
  if (n >= 0.01) return `$${n.toFixed(3)}`;
  return `$${n.toFixed(4)}`;
}

function fmtSpan(start?: string | null, end?: string | null): string | null {
  if (!start || !end) return null;
  const ms = Date.parse(end) - Date.parse(start);
  if (!Number.isFinite(ms) || ms < 0) return null;
  const s = Math.round(ms / 1000);
  if (s < 90) return `${s}s span`;
  const m = Math.round(s / 60);
  if (m < 90) return `${m}m span`;
  return `${+(m / 60).toFixed(1)}h span`;
}

const STATUS_LABEL: Record<Status, string> = {
  ready: "ready",
  analyzing: "analyzing",
  not_ingested: "not ingested",
};

// ─── pieces ───────────────────────────────────────────────────────

function tokenChips(t: Partial<Tokens>): HTMLElement {
  const parts: [string, number][] = [
    ["in", t.input ?? 0],
    ["out", t.output ?? 0],
    ["cache", (t.cache_read ?? 0) + (t.cache_creation ?? 0)],
    ["reason", t.reasoning ?? 0],
  ];
  return el(
    "div",
    { class: "tokens" },
    parts
      .filter(([, n]) => n > 0)
      .map(([label, n]) =>
        el("span", { class: "tok" }, [
          el("span", { class: "tok-n", text: fmtNum(n) }),
          el("span", { class: "tok-l", text: label }),
        ]),
      ),
  );
}

function nodeChip(n: TaxNode): HTMLElement {
  const mark = (n.emoji || n.letter || "•").trim() || "•";
  const chip = el("span", { class: "node", title: n.path || n.name }, [
    el("span", { class: "node-mark", text: mark }),
    el("span", { class: "node-name", text: n.name }),
  ]);
  if (n.color) chip.style.setProperty("--chip", n.color);
  return chip;
}

function statusPill(status: Status): HTMLElement {
  return el("span", { class: `pill pill-${status}` }, [
    el("span", { class: "dot" }),
    el("span", { text: STATUS_LABEL[status] }),
  ]);
}

function header(): HTMLElement {
  return el("div", { class: "brand" }, [
    el("span", { class: "logo", text: "◆" }),
    el("span", { class: "title", text: "Session insights" }),
  ]);
}

// ─── entry points ─────────────────────────────────────────────────

/** Render the insights card into `root`, replacing prior content. */
export function render(root: HTMLElement, ins: Insights): void {
  const status: Status = ins.status ?? "analyzing";
  const tokens = ins.tokens ?? {};
  const nodes = ins.taxonomy_nodes ?? [];

  const head = el("div", { class: "head" }, [header(), statusPill(status)]);
  const body = el("div", { class: "body" });

  if (status === "not_ingested") {
    body.append(
      el("div", { class: "empty" }, [
        el("div", { class: "empty-t", text: "Nothing ingested yet" }),
        el("div", {
          class: "empty-s",
          text: "Run a session with the modelstat daemon active, or `modelstat sync --session`. It'll appear here within seconds.",
        }),
      ]),
    );
  } else {
    body.append(
      el("div", { class: "hero" }, [
        el("div", { class: "metric" }, [
          el("span", { class: "metric-v", text: fmtUsd(ins.cost_usd ?? 0) }),
          el("span", { class: "metric-l", text: "assigned" }),
        ]),
        el("div", { class: "metric metric-r" }, [
          el("span", { class: "metric-v", text: fmtNum(tokens.total ?? 0) }),
          el("span", { class: "metric-l", text: "tokens" }),
        ]),
      ]),
      tokenChips(tokens),
    );

    if (nodes.length) {
      body.append(el("div", { class: "nodes" }, nodes.slice(0, 12).map(nodeChip)));
    } else if (status === "analyzing") {
      body.append(el("div", { class: "muted", text: "Detecting work types…" }));
    }
  }

  const footBits: string[] = [];
  if ((ins.segment_count ?? 0) > 0) {
    footBits.push(`${ins.segment_count} segment${ins.segment_count === 1 ? "" : "s"}`);
  }
  const span = fmtSpan(ins.started_at, ins.ended_at);
  if (span) footBits.push(span);
  if (status === "analyzing" && (ins.segments_pending ?? 0) > 0) {
    footBits.push(`${ins.segments_pending} pending`);
  }

  const foot = el("div", { class: "foot" }, [
    el("span", { class: "foot-l", text: footBits.join(" · ") }),
  ]);
  if (status === "analyzing") foot.append(el("span", { class: "spinner" }));

  root.replaceChildren(el("div", { class: `card card-${status}` }, [head, body, foot]));
}

/** Render a minimal error card (e.g. a malformed tool result). */
export function renderError(root: HTMLElement, message: string): void {
  root.replaceChildren(
    el("div", { class: "card" }, [
      el("div", { class: "head" }, [header()]),
      el("div", { class: "body" }, [el("div", { class: "muted", text: message })]),
    ]),
  );
}
