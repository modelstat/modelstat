import type React from "react";
import { useEffect, useState } from "react";
import { WEBLLM_CHAT_MODEL } from "@modelstat/daemon-core/pipeline";

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

type Snap = {
  auth: AuthStatus;
  syncEnabled: boolean;
  claim_code?: string | null;
  claim_url?: string | null;
};

export function Options(): React.JSX.Element {
  const [snap, setSnap] = useState<Snap | null>(null);
  const [aiStatus, setAiStatus] = useState<{
    promptApi: "unavailable" | "downloadable" | "downloading" | "available" | "unknown";
    webllmEnabled: boolean;
    webllmModel: string;
    embeddingsReady: boolean;
  }>({
    promptApi: "unknown",
    webllmEnabled: false,
    webllmModel: WEBLLM_CHAT_MODEL,
    embeddingsReady: false,
  });

  const refresh = () =>
    chrome.runtime.sendMessage({ kind: "popup-snapshot-request" }, (reply) => {
      if (reply)
        setSnap({
          auth: reply.auth,
          syncEnabled: reply.syncEnabled,
          claim_code: reply.claim_code,
          claim_url: reply.claim_url,
        });
    });

  useEffect(() => {
    refresh();
    const t = window.setInterval(refresh, 2000);
    chrome.storage.local.get(
      ["aiPromptApi", "aiWebllmEnabled", "aiWebllmModel", "aiEmbeddingsReady"],
      (v) => {
        setAiStatus({
          promptApi: (v.aiPromptApi as typeof aiStatus.promptApi) ?? "unknown",
          webllmEnabled: !!v.aiWebllmEnabled,
          webllmModel: (v.aiWebllmModel as string) ?? WEBLLM_CHAT_MODEL,
          embeddingsReady: !!v.aiEmbeddingsReady,
        });
      },
    );
    return () => window.clearInterval(t);
  }, []);

  const claim = () => chrome.runtime.sendMessage({ kind: "auth-open-claim" });
  const reset = () => {
    if (!confirm("Reset identity? This orphans any unclaimed data on this device and mints a fresh identity. Claimed devices keep their account.")) return;
    chrome.runtime.sendMessage({ kind: "auth-disconnect" }, refresh);
  };
  const toggleSync = (enabled: boolean) =>
    chrome.runtime.sendMessage({ kind: "sync-toggle", enabled }, refresh);

  if (!snap) return <div className="p-8 text-slate-400">loading…</div>;

  return (
    <div className="max-w-2xl mx-auto p-8 space-y-8">
      <header>
        <h1 className="text-2xl font-bold">modelstat — settings</h1>
        <p className="text-sm text-slate-400 mt-1">
          Track token usage across ChatGPT, Claude, Gemini, and Grok. Local-first.
        </p>
      </header>

      <section className="rounded-lg bg-slate-900 border border-slate-800 p-5 space-y-4">
        <h2 className="font-semibold">Device</h2>
        {snap.auth.kind === "registering" && (
          <div className="text-sm text-slate-400">Registering this browser with modelstat…</div>
        )}
        {snap.auth.kind === "error" && (
          <div className="space-y-2">
            <div className="text-sm text-rose-300">Registration failed</div>
            <div className="text-xs text-slate-400">{snap.auth.message}</div>
            <button
              type="button"
              onClick={() => chrome.runtime.sendMessage({ kind: "auth-refresh" }, refresh)}
              className="px-3 py-1.5 rounded border border-slate-700 text-xs hover:bg-slate-800"
            >
              Retry
            </button>
          </div>
        )}
        {snap.auth.kind === "unclaimed" && (
          <div className="space-y-3">
            <div>
              <div className="text-sm text-slate-100">Unclaimed · working under the free tier</div>
              <div className="text-xs text-slate-500 mt-1">
                device: <span className="font-mono">{snap.auth.deviceId.slice(0, 12)}…</span>
              </div>
              <div className="text-xs text-slate-500">
                claim code: <span className="font-mono text-slate-300">{snap.auth.claimCode}</span>
              </div>
            </div>
            <button
              type="button"
              onClick={claim}
              className="w-full rounded px-3 py-2 text-sm font-semibold text-slate-950"
              style={{ background: "linear-gradient(135deg, #FCD34D, #F59E0B)" }}
            >
              Claim this device →
            </button>
            <p className="text-xs text-slate-500 leading-snug">
              Claiming moves all data collected on this device into your modelstat
              account. If you uninstall before claiming, the data expires in 30 days.
            </p>
          </div>
        )}
        {snap.auth.kind === "claimed" && (
          <div className="flex items-center justify-between">
            <div>
              <div className="text-sm text-slate-100">Claimed</div>
              <div className="text-xs text-slate-500 mt-1">
                device: <span className="font-mono">{snap.auth.deviceId.slice(0, 12)}…</span>
              </div>
              {snap.auth.userId && (
                <div className="text-xs text-slate-500">
                  user: <span className="font-mono">{snap.auth.userId.slice(0, 12)}…</span>
                </div>
              )}
            </div>
            <a
              href="https://modelstat.ai/dashboard/devices"
              target="_blank"
              rel="noreferrer"
              className="text-xs text-slate-400 hover:text-slate-200"
            >
              dashboard →
            </a>
          </div>
        )}
        <div className="pt-2 border-t border-slate-800">
          <button
            type="button"
            onClick={reset}
            className="text-[11px] text-slate-500 hover:text-rose-400"
          >
            reset identity (mint a fresh unclaimed device)
          </button>
        </div>
      </section>

      <section className="rounded-lg bg-slate-900 border border-slate-800 p-5 space-y-4">
        <h2 className="font-semibold">Cloud sync</h2>
        <label className="flex items-center justify-between">
          <div>
            <div className="text-sm">Sync usage to modelstat cloud</div>
            <div className="text-xs text-slate-500">
              Sends tokens + model + timestamps only. No chat content ever leaves your device.
              Works on unclaimed devices too — your data goes into a pending org you can claim
              later from the device page.
            </div>
          </div>
          <input
            type="checkbox"
            checked={snap.syncEnabled}
            onChange={(e) => toggleSync(e.target.checked)}
            className="w-5 h-5"
          />
        </label>
      </section>

      <section className="rounded-lg bg-slate-900 border border-slate-800 p-5 space-y-4">
        <h2 className="font-semibold">On-device AI features</h2>
        <p className="text-xs text-slate-500">
          All AI processing happens on your machine. No conversation content is ever sent to any
          server, including modelstat.
        </p>

        <div className="space-y-3">
          <Row
            label="Chrome built-in (Gemini Nano)"
            status={aiStatus.promptApi}
            description="Zero download. Used when available for conversation summaries."
          />
          <Row
            label="Embeddings (bge-small-en-v1.5)"
            status={aiStatus.embeddingsReady ? "available" : "downloadable"}
            description="~33 MB. Used to categorize conversations and detect near-duplicates."
          />
          <div>
            <label className="flex items-center justify-between">
              <div>
                <div className="text-sm">Downloaded summariser (WebLLM)</div>
                <div className="text-xs text-slate-500">
                  Used when Gemini Nano isn't available. Default is the same
                  Qwen 4B the CLI runs (~2.4 GB); pick a lighter model below on
                  a constrained GPU.
                </div>
              </div>
              <input
                type="checkbox"
                checked={aiStatus.webllmEnabled}
                onChange={(e) => {
                  chrome.storage.local.set({ aiWebllmEnabled: e.target.checked });
                  setAiStatus({ ...aiStatus, webllmEnabled: e.target.checked });
                }}
                className="w-5 h-5"
              />
            </label>
            {aiStatus.webllmEnabled && (
              <div className="pl-4 mt-2 space-y-2">
                <label className="flex items-center gap-2 text-xs">
                  <input
                    type="radio"
                    name="webllm-model"
                    checked={aiStatus.webllmModel === WEBLLM_CHAT_MODEL}
                    onChange={() => {
                      const model = WEBLLM_CHAT_MODEL;
                      chrome.storage.local.set({ aiWebllmModel: model });
                      setAiStatus({ ...aiStatus, webllmModel: model });
                    }}
                  />
                  Qwen3-4B-Instruct (~2.4 GB, same as the CLI — recommended)
                </label>
                <label className="flex items-center gap-2 text-xs">
                  <input
                    type="radio"
                    name="webllm-model"
                    checked={aiStatus.webllmModel === "Qwen3-0.6B-Instruct-q4f16_1-MLC"}
                    onChange={() => {
                      const model = "Qwen3-0.6B-Instruct-q4f16_1-MLC";
                      chrome.storage.local.set({ aiWebllmModel: model });
                      setAiStatus({ ...aiStatus, webllmModel: model });
                    }}
                  />
                  Qwen3-0.6B-Instruct (~380 MB, lighter)
                </label>
                <label className="flex items-center gap-2 text-xs">
                  <input
                    type="radio"
                    name="webllm-model"
                    checked={aiStatus.webllmModel === "SmolLM2-360M-Instruct-q4f16_1-MLC"}
                    onChange={() => {
                      const model = "SmolLM2-360M-Instruct-q4f16_1-MLC";
                      chrome.storage.local.set({ aiWebllmModel: model });
                      setAiStatus({ ...aiStatus, webllmModel: model });
                    }}
                  />
                  SmolLM2-360M-Instruct (~250 MB, smaller)
                </label>
              </div>
            )}
          </div>
        </div>
      </section>

      <section className="rounded-lg bg-slate-900 border border-slate-800 p-5 space-y-3">
        <h2 className="font-semibold">Privacy</h2>
        <ul className="text-xs text-slate-400 space-y-1.5">
          <li>· Default: everything stays on this device, in IndexedDB.</li>
          <li>· Cloud sync, if enabled, sends tokens/model/timestamps only — never chat content.</li>
          <li>· Adapter configs are signed (Ed25519) and verified locally before use.</li>
          <li>· No analytics on you. No sale of data. Ever.</li>
        </ul>
      </section>
    </div>
  );
}

function Row(props: { label: string; status: string; description: string }): React.JSX.Element {
  const color =
    props.status === "available"
      ? "bg-emerald-500"
      : props.status === "downloading"
        ? "bg-amber-500"
        : props.status === "downloadable"
          ? "bg-sky-500"
          : "bg-slate-600";
  return (
    <div className="flex items-center justify-between">
      <div>
        <div className="text-sm flex items-center gap-2">
          <span className={`w-2 h-2 rounded-full ${color}`} />
          {props.label}
        </div>
        <div className="text-xs text-slate-500 pl-4">{props.description}</div>
      </div>
      <span className="text-xs text-slate-500 uppercase tracking-wider">{props.status}</span>
    </div>
  );
}
