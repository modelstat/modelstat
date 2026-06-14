/**
 * Bridge: ISOLATED-world content script ↔ MAIN-world injector.
 *
 * The MAIN-world script is injected via manifest content_scripts with
 * `"world": "MAIN"` and runs at `document_start` alongside this script
 * — no fetch/inject dance needed. Our only job here is to listen for
 * postMessages tagged BRIDGE_TAG from the same origin and forward the
 * frame payload to the SW.
 */

import { BRIDGE_TAG } from "@/common/config.js";

export type MainFrame =
  | {
      type: "request";
      id: string;
      url: string;
      method: string;
      requestBody: string | null;
      startedAt: number;
    }
  | {
      type: "response_start";
      id: string;
      status: number;
      contentType: string | null;
    }
  | {
      type: "response_chunk";
      id: string;
      chunks: string[];
    }
  | {
      type: "response_end";
      id: string;
      endedAt: number;
      aborted: boolean;
    };

export type FrameHandler = (frame: MainFrame) => void;

let handler: FrameHandler | null = null;
let installed = false;

export function onFrame(fn: FrameHandler): void {
  handler = fn;
}

export async function installMainWorld(): Promise<void> {
  if (installed) return;
  installed = true;

  window.addEventListener("message", (ev: MessageEvent) => {
    // Origin check: only accept from the current page's origin.
    if (ev.source !== window || ev.origin !== window.location.origin) return;
    const data = ev.data as { tag?: string; frame?: MainFrame } | null;
    if (!data || data.tag !== BRIDGE_TAG || !data.frame) return;
    handler?.(data.frame);
  });
}
