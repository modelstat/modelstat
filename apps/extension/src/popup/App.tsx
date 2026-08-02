import type React from "react";
import { useEffect, useState } from "react";

type TodayEntry = { agent: string; tokens: number; model: string | null };
type AuthStatus =
  | { kind: "registering" }
  | {
      kind: "unclaimed";
      deviceId: string;
      deviceUuid: string;
      claimCode: string;
      claimUrl: string;
      claimExpiresAt: string | null;
    }
  | { kind: "claimed"; deviceId: string; deviceUuid: string; userId: string | null }
  | { kind: "error"; message: string };

type Counters = {
  frames: number;
  matches: number;
  dom_events: number;
  messages: number;
  events: number;
  heartbeats: number;
  ingested: number;
  last_frame_at: number | null;
  last_event_at: number | null;
};

type Session = {
  session_id: string;
  agent: string;
  model: string | null;
  conversation_id: string;
  tokens: number;
  messages: number;
  updated_at: string;
};

type Snapshot = {
  today: TodayEntry[];
  unsynced: number;
  auth: AuthStatus;
  syncEnabled: boolean;
  counters?: Counters;
  sessions?: Session[];
  pending_by_conversation?: Record<string, { tokens: number; count: number }>;
  /** Last time the ACTIVE tab emitted a network frame. null = never
   * (i.e. content script not attached, or user hasn't sent a message yet). */
  active_tab_last_frame_at?: number | null;
  claim_code?: string | null;
  claim_url?: string | null;
  recent_urls?: Array<{ url: string; method: string; matched: boolean; at: number }>;
};

type ActiveTab = {
  agent: string | null;
  agent_label: string | null;
  color: string | null;
  text_color: string | null;
  tab_id: number | null;
  url: string | null;
};

const PROVIDER_BY_HOST: Record<string, { agent: string; label: string; color: string; text: string }> = {
  "chatgpt.com": { agent: "chatgpt_web", label: "ChatGPT", color: "#10A37F", text: "#fff" },
  "chat.openai.com": { agent: "chatgpt_web", label: "ChatGPT", color: "#10A37F", text: "#fff" },
  "claude.ai": { agent: "claude_web", label: "Claude", color: "#D97757", text: "#fff" },
  "gemini.google.com": { agent: "gemini_web", label: "Gemini", color: "#4285F4", text: "#fff" },
  "grok.com": { agent: "grok_web", label: "Grok", color: "#E4E4E7", text: "#0a0a0a" },
  "x.com": { agent: "grok_web", label: "Grok", color: "#E4E4E7", text: "#0a0a0a" },
};

function detectActiveTab(): Promise<ActiveTab> {
  return new Promise((resolve) => {
    chrome.tabs.query({ active: true, currentWindow: true }, (tabs) => {
      const tab = tabs[0];
      if (!tab?.url || tab.id == null) {
        resolve({ agent: null, agent_label: null, color: null, text_color: null, tab_id: null, url: null });
        return;
      }
      try {
        const host = new URL(tab.url).hostname.toLowerCase();
        const p = PROVIDER_BY_HOST[host];
        if (p) {
          resolve({
            agent: p.agent,
            agent_label: p.label,
            color: p.color,
            text_color: p.text,
            tab_id: tab.id,
            url: tab.url,
          });
          return;
        }
      } catch {
        /* ignore */
      }
      resolve({ agent: null, agent_label: null, color: null, text_color: null, tab_id: tab.id, url: tab.url });
    });
  });
}

const AGENT_LABELS: Record<string, string> = {
  chatgpt_web: "ChatGPT",
  claude_web: "Claude",
  gemini_web: "Gemini",
  grok_web: "Grok",
};

function fmtTokens(n: number): string {
  if (n < 1_000) return String(n);
  if (n < 1_000_000) return `${(n / 1_000).toFixed(1)}K`;
  if (n < 1_000_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  return `${(n / 1_000_000_000).toFixed(2)}B`;
}

function useSnapshot(activeTabId: number | null) {
  const [snap, setSnap] = useState<Snapshot | null>(null);
  const refresh = () =>
    chrome.runtime.sendMessage(
      { kind: "popup-snapshot-request", active_tab_id: activeTabId ?? undefined },
      (reply: Snapshot) => {
        if (reply) setSnap(reply);
      },
    );
  useEffect(() => {
    refresh();
    const t = window.setInterval(refresh, 1500);
    return () => window.clearInterval(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeTabId]);
  return { snap, refresh };
}

export function App(): React.JSX.Element {
  const [activeTab, setActiveTab] = useState<ActiveTab | null>(null);
  useEffect(() => {
    detectActiveTab().then(setActiveTab);
  }, []);
  const { snap, refresh } = useSnapshot(activeTab?.tab_id ?? null);

  if (!snap) {
    return <div className="p-6 text-slate-400 text-sm">loading…</div>;
  }

  const totalTokens = snap.today.reduce((s, e) => s + e.tokens, 0);

  const statusDot =
    snap.auth.kind === "claimed" && snap.syncEnabled
      ? { color: "bg-emerald-500", label: "syncing · claimed" }
      : snap.auth.kind === "claimed"
        ? { color: "bg-sky-500", label: "claimed" }
        : snap.auth.kind === "unclaimed"
          ? { color: "bg-amber-400", label: "unclaimed" }
          : snap.auth.kind === "error"
            ? { color: "bg-rose-500", label: "error" }
            : { color: "bg-slate-500 animate-pulse", label: "registering" };

  return (
    <div className="p-4 space-y-4">
      <header className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <div
            className="w-6 h-6 rounded"
            style={{ background: "linear-gradient(135deg, #64ECAC, #00B1C3)" }}
          />
          <span className="font-semibold tracking-tight">modelstat web</span>
        </div>
        <div className="flex items-center gap-2 text-xs text-slate-400">
          <span className={`w-2 h-2 rounded-full ${statusDot.color}`} />
          {statusDot.label}
        </div>
      </header>

      {snap.auth.kind === "unclaimed" && snap.claim_url && (
        <ClaimBanner claimUrl={snap.claim_url} claimCode={snap.claim_code ?? ""} />
      )}
      {snap.auth.kind === "error" && <AuthErrorPanel message={snap.auth.message} />}
      {snap.auth.kind === "registering" && (
        <div className="rounded-lg border border-slate-800 bg-slate-900/50 p-3 text-xs text-slate-400">
          Registering this browser with modelstat…
        </div>
      )}

      <section className="rounded-lg bg-slate-900 border border-slate-800 p-3">
        <div className="text-xs text-slate-400 uppercase tracking-wide">today</div>
        <div className="flex items-baseline justify-between mt-1">
          <span className="text-2xl font-bold tabular-nums">{fmtTokens(totalTokens)}</span>
        </div>
        <div className="text-xs text-slate-500 mt-0.5">tokens · $ in the dashboard</div>
      </section>

      {activeTab?.agent && <ActiveTabTracker active={activeTab} snap={snap} />}

      <section className="space-y-1">
        {snap.today.length === 0 && !activeTab?.agent && (
          <div className="space-y-3 px-1">
            <div className="text-xs text-slate-400">
              Pick a provider to start tracking. We won't log you in — sign in
              there if you haven't already, then come back here.
            </div>
            <div className="grid grid-cols-2 gap-2">
              <LaunchButton label="ChatGPT" color="#10A37F" url="https://chatgpt.com" />
              <LaunchButton label="Claude" color="#D97757" url="https://claude.ai" />
              <LaunchButton label="Gemini" color="#4285F4" url="https://gemini.google.com" />
              <LaunchButton label="Grok" color="#E4E4E7" textColor="#0a0a0a" url="https://grok.com" />
            </div>
          </div>
        )}
        {snap.today.map((entry) => (
          <div
            key={entry.agent}
            className="flex items-center justify-between px-2 py-1.5 rounded hover:bg-slate-900"
          >
            <div className="flex items-center gap-2">
              <span className="text-sm">{AGENT_LABELS[entry.agent] ?? entry.agent}</span>
              {entry.model && (
                <span className="text-[10px] text-slate-500 border border-slate-700 rounded px-1">
                  {entry.model}
                </span>
              )}
            </div>
            <div className="flex items-baseline gap-3 text-xs tabular-nums">
              <span className="text-slate-300">{fmtTokens(entry.tokens)}</span>
            </div>
          </div>
        ))}
      </section>

      {(snap.sessions?.length ?? 0) > 0 && <SessionsList sessions={snap.sessions!} />}
      <PipelineCounters
        c={
          snap.counters ?? {
            frames: 0,
            matches: 0,
            dom_events: 0,
            messages: 0,
            events: 0,
            heartbeats: 0,
            ingested: 0,
            last_frame_at: null,
            last_event_at: null,
          }
        }
        unsynced={snap.unsynced}
      />
      {snap.counters &&
        snap.counters.frames > 0 &&
        snap.counters.matches === 0 &&
        (snap.recent_urls?.length ?? 0) > 0 && <UnmatchedUrlsDebug urls={snap.recent_urls!} />}

      <footer className="flex items-center justify-between pt-2 border-t border-slate-800 text-xs">
        <DashboardLink snap={snap} />
        <button
          type="button"
          onClick={() => chrome.runtime.openOptionsPage()}
          className="text-slate-400 hover:text-slate-200"
        >
          settings
        </button>
      </footer>
    </div>
  );
}

function ClaimBanner({ claimUrl, claimCode }: { claimUrl: string; claimCode: string }): React.JSX.Element {
  const open = () => {
    chrome.runtime.sendMessage({ kind: "auth-open-claim" });
    window.close();
  };
  return (
    <section
      className="rounded-lg border p-3 space-y-2"
      style={{
        borderColor: "rgba(252,211,77,0.25)",
        background: "linear-gradient(135deg, rgba(252,211,77,0.08), rgba(245,158,11,0.06))",
      }}
    >
      <div className="flex items-start gap-3">
        <div className="flex-1 min-w-0">
          <div className="text-sm font-semibold text-slate-100">Claim this device</div>
          <div className="text-[11px] text-slate-300 leading-snug">
            Tracking works locally. Sign in to lift free-tier limits, sync across
            devices, and keep history if you uninstall.
          </div>
        </div>
        <button
          type="button"
          onClick={open}
          className="shrink-0 self-center px-3 py-1.5 rounded text-xs font-semibold text-slate-950 whitespace-nowrap"
          style={{ background: "linear-gradient(135deg, #FCD34D, #F59E0B)" }}
        >
          Claim →
        </button>
      </div>
      {claimCode && (
        <div className="text-[10px] text-slate-500 font-mono">
          code: <span className="text-slate-300">{claimCode}</span>
        </div>
      )}
    </section>
  );
}

function AuthErrorPanel({ message }: { message: string }): React.JSX.Element {
  const retry = () => chrome.runtime.sendMessage({ kind: "auth-refresh" });
  return (
    <section className="rounded-lg border border-rose-500/30 bg-rose-500/10 p-3 space-y-2">
      <div className="text-sm font-semibold text-rose-100">Registration failed</div>
      <div className="text-[11px] text-rose-200/90 leading-snug">
        {describeAuthError(message)}
      </div>
      <button
        type="button"
        onClick={retry}
        className="w-full rounded border border-rose-500/40 bg-rose-500/10 px-3 py-1.5 text-xs text-rose-100 hover:bg-rose-500/20"
      >
        Retry
      </button>
    </section>
  );
}

function describeAuthError(raw: string): string {
  const lc = raw.toLowerCase();
  if (lc.includes("failed to fetch") || lc.includes("networkerror") || lc.includes("load failed")) {
    return "Can't reach modelstat.ai. Check your connection and retry.";
  }
  if (lc.includes("429")) return "Rate limited. Wait a minute and retry.";
  return raw;
}

function DashboardLink({ snap }: { snap: Snapshot }): React.JSX.Element {
  // Unclaimed → public /d/:claim_code page (no sign-in needed, shows
  // THIS device's data). Claimed → the real dashboard.
  if (snap.auth.kind === "unclaimed" && snap.claim_url) {
    return (
      <a
        href={snap.claim_url}
        target="_blank"
        rel="noreferrer"
        className="text-slate-400 hover:text-slate-200"
      >
        device page →
      </a>
    );
  }
  return (
    <a
      href="https://modelstat.ai/dashboard"
      target="_blank"
      rel="noreferrer"
      className="text-slate-400 hover:text-slate-200"
    >
      dashboard →
    </a>
  );
}

function ActiveTabTracker({ active, snap }: { active: ActiveTab; snap: Snapshot }): React.JSX.Element {
  const reload = async () => {
    if (active.tab_id != null) await chrome.tabs.reload(active.tab_id);
    window.close();
  };

  const conversationId = extractConversationId(active.url);
  const session = conversationId
    ? snap.sessions?.find((s) => s.conversation_id === conversationId && s.agent === active.agent)
    : undefined;
  const pending = conversationId ? snap.pending_by_conversation?.[conversationId] : undefined;

  // Attachment is TAB-SCOPED — did this specific tab ever emit a
  // network frame? If null, the content script hasn't run on this
  // tab (usually means the tab loaded before the extension installed).
  const tabAttachedAt = snap.active_tab_last_frame_at;
  const neverAttached = tabAttachedAt == null;
  const tabLiveMs = tabAttachedAt != null ? Date.now() - tabAttachedAt : Infinity;
  const tabLive = tabLiveMs < 10_000;

  // State machine → three visible states:
  //   not-attached:  big "Attach tracker" CTA (user hasn't refreshed since install)
  //   attached-idle: "Tracker attached, send a message" hint
  //   attached-live: "LIVE · N frames" with conversation data
  const state: "not-attached" | "attached-idle" | "attached-live" = neverAttached
    ? "not-attached"
    : tabLive
      ? "attached-live"
      : "attached-idle";

  const statePill =
    state === "not-attached"
      ? { label: "NOT ATTACHED", color: "text-amber-300", dot: "bg-amber-400" }
      : state === "attached-live"
        ? { label: "LIVE", color: "text-emerald-300", dot: "bg-emerald-400 animate-pulse" }
        : { label: "ATTACHED · IDLE", color: "text-sky-300", dot: "bg-sky-400" };

  const borderColor =
    state === "not-attached" ? "border-amber-500/30" : "border-emerald-500/25";
  const bgTint = state === "not-attached" ? "bg-amber-500/5" : "bg-emerald-500/5";

  return (
    <section className={`rounded-lg border ${borderColor} ${bgTint} p-3 space-y-2`}>
      <div className="flex items-center gap-2">
        <span
          className="inline-flex h-6 w-6 items-center justify-center rounded text-[11px] font-bold"
          style={{ background: active.color ?? "#fff", color: active.text_color ?? "#000" }}
        >
          {active.agent_label?.[0] ?? "?"}
        </span>
        <span className="text-sm font-semibold text-slate-50">Tracking {active.agent_label}</span>
        <span className={`ml-auto inline-flex items-center gap-1 text-[10px] uppercase tracking-wider ${statePill.color}`}>
          <span className={`w-1.5 h-1.5 rounded-full ${statePill.dot}`} />
          {statePill.label}
        </span>
      </div>

      {state === "not-attached" && (
        <>
          <p className="text-[11px] text-slate-300 leading-snug">
            This tab loaded before modelstat was installed. Click below — we'll
            refresh the page so the tracker can attach at the right moment.
            (Your unsent draft is preserved by the site.)
          </p>
          <button
            type="button"
            onClick={reload}
            className="w-full rounded px-3 py-2 text-sm font-semibold text-slate-950"
            style={{ background: "linear-gradient(135deg, #FCD34D, #F59E0B)" }}
          >
            Attach tracker →
          </button>
        </>
      )}

      {state === "attached-idle" && (
        <p className="text-[11px] text-slate-300 leading-snug">
          Tracker attached. Send a message on this page and tokens appear here
          within seconds.
        </p>
      )}

      {state === "attached-live" && session && (
        <div className="flex items-baseline justify-between">
          <div className="text-xs text-slate-400">
            this conversation · <span className="text-slate-200">{session.messages} msgs</span>
            {session.model && <span className="text-slate-500"> · {session.model}</span>}
          </div>
          <div className="text-sm tabular-nums">
            <span className="text-slate-50 font-semibold">{fmtTokens(session.tokens)}</span>
          </div>
        </div>
      )}

      {state === "attached-live" && !session && pending && (
        <div className="text-xs text-slate-300">
          streaming… <span className="text-slate-50 font-semibold">{fmtTokens(pending.tokens)} tokens</span>{" "}
          across <span className="text-slate-200">{pending.count}</span> in-flight msg
          {pending.count === 1 ? "" : "s"}
        </div>
      )}

      {state === "attached-live" && !session && !pending && (
        <div className="text-[11px] text-slate-400">
          Captured {snap.counters?.frames ?? 0} network frames on this tab so far.
          Waiting for the next message.
        </div>
      )}
    </section>
  );
}

function extractConversationId(url: string | null): string | null {
  if (!url) return null;
  try {
    const u = new URL(url);
    // ChatGPT:   /c/<uuid>
    // Claude:    /chat/<uuid>
    // Gemini:    /app/<id>
    // Grok:      /chat/<id>
    const m = /\/(?:c|chat|app)\/([A-Za-z0-9-_]+)/.exec(u.pathname);
    return m ? m[1]! : null;
  } catch {
    return null;
  }
}

function SessionsList({ sessions }: { sessions: Session[] }): React.JSX.Element {
  return (
    <section className="space-y-1">
      <div className="text-[10px] uppercase tracking-wide text-slate-500 px-1 pt-1">
        Recent conversations
      </div>
      <div className="space-y-1">
        {sessions.map((s) => {
          const meta = PROVIDER_BY_AGENT[s.agent] ?? { label: s.agent, color: "#64748B", text: "#fff" };
          return (
            <div
              key={s.session_id}
              className="flex items-center gap-2 rounded border border-slate-800 bg-slate-900/50 px-2 py-1.5 text-xs"
            >
              <span
                className="inline-flex h-5 w-5 items-center justify-center rounded text-[10px] font-bold shrink-0"
                style={{ background: meta.color, color: meta.text }}
              >
                {meta.label[0]}
              </span>
              <div className="flex-1 min-w-0">
                <div className="text-slate-200 truncate">
                  {meta.label}
                  {s.model && <span className="text-slate-500"> · {s.model}</span>}
                </div>
                <div className="text-[10px] text-slate-500">
                  {s.messages} msg{s.messages === 1 ? "" : "s"} · {fmtAgo(s.updated_at)}
                </div>
              </div>
              <div className="text-right tabular-nums shrink-0">
                <div className="text-slate-200">{fmtTokens(s.tokens)}</div>
              </div>
            </div>
          );
        })}
      </div>
    </section>
  );
}

const PROVIDER_BY_AGENT: Record<string, { label: string; color: string; text: string }> = {
  chatgpt_web: { label: "ChatGPT", color: "#10A37F", text: "#fff" },
  claude_web: { label: "Claude", color: "#D97757", text: "#fff" },
  gemini_web: { label: "Gemini", color: "#4285F4", text: "#fff" },
  grok_web: { label: "Grok", color: "#E4E4E7", text: "#0a0a0a" },
};

function fmtAgo(iso: string): string {
  const s = Math.max(0, Math.floor((Date.now() - new Date(iso).getTime()) / 1000));
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

function UnmatchedUrlsDebug({
  urls,
}: {
  urls: Array<{ url: string; method: string; matched: boolean; at: number }>;
}): React.JSX.Element {
  // Capture is working (frames > 0) but nothing matched the adapter
  // regex — show the user / us what URLs are actually being hit so we
  // can decide whether to loosen the regex. Shown only in the no-match
  // diagnostic case; invisible once matches > 0.
  return (
    <section className="rounded border border-amber-500/25 bg-amber-500/5 p-2 space-y-1">
      <div className="text-[10px] uppercase tracking-wide text-amber-300 mb-1">
        No URL matched the adapter · recent captures
      </div>
      <div className="space-y-0.5 max-h-40 overflow-y-auto font-mono text-[10px]">
        {urls.map((u) => (
          <div
            key={`${u.method}::${u.url}`}
            className={`flex gap-1.5 ${u.matched ? "text-emerald-300" : "text-slate-400"}`}
          >
            <span className="shrink-0 w-9 text-slate-500">{u.method}</span>
            <span className="truncate">{u.url}</span>
          </div>
        ))}
      </div>
      <div className="text-[10px] text-slate-500 leading-snug">
        Paste one of these into a bug report if tokens aren't appearing.
      </div>
    </section>
  );
}

function PipelineCounters({ c, unsynced }: { c: Counters; unsynced: number }): React.JSX.Element {
  // Surface the pipeline health so users can see WHERE flow stops when
  // something's off: frames arriving? matched against adapter? messages
  // extracted? events finalised? heartbeats landing? events synced?
  return (
    <section className="rounded border border-slate-800 bg-slate-900/40 p-2">
      <div className="text-[10px] uppercase tracking-wide text-slate-500 mb-1.5">
        Pipeline
      </div>
      <div className="grid grid-cols-4 gap-1.5 text-center">
        <Counter label="frames" value={c.frames} />
        <Counter label="matches" value={c.matches} />
        <Counter label="msgs" value={c.messages} />
        <Counter label="events" value={c.events} />
        <Counter label="dom" value={c.dom_events} />
        <Counter label="♥" value={c.heartbeats} />
        <Counter label="sent" value={c.ingested} />
        <Counter label="queue" value={unsynced} highlight={unsynced > 0} />
      </div>
    </section>
  );
}

function Counter({ label, value, highlight }: { label: string; value: number; highlight?: boolean }): React.JSX.Element {
  return (
    <div className="flex flex-col items-center">
      <span
        className={`text-sm tabular-nums ${highlight ? "text-amber-300" : "text-slate-200"}`}
      >
        {value}
      </span>
      <span className="text-[9px] uppercase tracking-wider text-slate-500">{label}</span>
    </div>
  );
}

function LaunchButton({
  label,
  color,
  textColor = "#fff",
  url,
}: {
  label: string;
  color: string;
  textColor?: string;
  url: string;
}): React.JSX.Element {
  const launch = async () => {
    const host = new URL(url).hostname;
    // Find any existing tab on that host. If the extension was
    // installed AFTER the tab was loaded, the MAIN-world fetch patch
    // missed its document_start window and nothing is being captured
    // — a reload fixes it. We focus + reload in one call; popup closes
    // so the user lands on the refreshed chat page.
    try {
      const tabs = await chrome.tabs.query({ url: [`*://${host}/*`, `*://*.${host}/*`] });
      const tab = tabs[0];
      if (tab?.id != null) {
        await chrome.tabs.update(tab.id, { active: true });
        if (tab.windowId != null) await chrome.windows.update(tab.windowId, { focused: true });
        await chrome.tabs.reload(tab.id);
        window.close();
        return;
      }
    } catch {
      /* fall through to new tab */
    }
    await chrome.tabs.create({ url });
    window.close();
  };
  return (
    <button
      type="button"
      onClick={launch}
      className="flex items-center gap-2 rounded-lg border border-slate-800 bg-slate-900 px-3 py-2 text-xs hover:bg-slate-800 transition-colors"
    >
      <span
        className="inline-flex h-6 w-6 items-center justify-center rounded text-[11px] font-bold"
        style={{ background: color, color: textColor }}
      >
        {label[0]}
      </span>
      <span className="flex-1 text-left text-slate-200">Analyze {label}</span>
      <span className="text-slate-500">→</span>
    </button>
  );
}
