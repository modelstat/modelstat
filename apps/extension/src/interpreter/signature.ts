/**
 * Ed25519 signature verification for signed adapter configs.
 *
 * The extension bundles a public key at public/pubkey.ed25519 (raw
 * 32-byte base64). At runtime it fetches the signed envelope:
 *
 *   { config: <base64 canonical-json>, signature: <base64>,
 *     signed_at: <iso>, adapter_version: <n> }
 *
 * Steps:
 *   1. Load pubkey via chrome.runtime.getURL + fetch + atob → 32 bytes
 *   2. Import as CryptoKey (Ed25519, verify usage)
 *   3. Base64-decode signature + config
 *   4. crypto.subtle.verify(algorithm, key, signature, config-bytes)
 *   5. JSON.parse the config bytes as utf-8
 *   6. Zod-validate against AdapterConfig
 *
 * A compromised server cannot forge a valid config — the extension
 * will reject mismatched signatures and fall back to the bundled
 * adapter (last-known-good).
 */

import { AdapterConfig, SignedAdapter } from "@modelstat/adapters-protocol";
import { createLogger } from "@/common/logger.js";

const log = createLogger("signature");

let keyPromise: Promise<CryptoKey> | null = null;

function base64ToBytes(b64: string): Uint8Array<ArrayBuffer> {
  const bin = atob(b64);
  const buf = new ArrayBuffer(bin.length);
  const out = new Uint8Array(buf);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

async function loadPublicKey(): Promise<CryptoKey> {
  if (keyPromise) return keyPromise;
  keyPromise = (async () => {
    const url = chrome.runtime.getURL("public/pubkey.ed25519");
    const res = await fetch(url);
    const b64 = (await res.text()).trim();
    const raw = base64ToBytes(b64);
    if (raw.length !== 32) {
      throw new Error(`bundled pubkey must be 32 bytes, got ${raw.length}`);
    }
    return crypto.subtle.importKey("raw", raw, { name: "Ed25519" }, false, ["verify"]);
  })();
  return keyPromise;
}

export type VerifyResult =
  | { ok: true; adapter: AdapterConfig }
  | { ok: false; reason: string };

export async function verifySignedAdapter(envelope: unknown): Promise<VerifyResult> {
  const parsed = SignedAdapter.safeParse(envelope);
  if (!parsed.success) return { ok: false, reason: "envelope_invalid" };
  const env = parsed.data;

  let key: CryptoKey;
  try {
    key = await loadPublicKey();
  } catch (e) {
    log.error("pubkey load failed", e);
    return { ok: false, reason: "pubkey_load_failed" };
  }

  const configBytes = base64ToBytes(env.config);
  const sigBytes = base64ToBytes(env.signature);
  let valid = false;
  try {
    valid = await crypto.subtle.verify({ name: "Ed25519" }, key, sigBytes, configBytes);
  } catch (e) {
    log.warn("verify threw", e);
    return { ok: false, reason: "verify_threw" };
  }
  if (!valid) return { ok: false, reason: "signature_mismatch" };

  let parsedConfig: unknown;
  try {
    parsedConfig = JSON.parse(new TextDecoder().decode(configBytes));
  } catch {
    return { ok: false, reason: "config_not_json" };
  }
  const ac = AdapterConfig.safeParse(parsedConfig);
  if (!ac.success) {
    log.warn("adapter schema rejected", ac.error.issues);
    return { ok: false, reason: "config_schema_invalid" };
  }
  if (ac.data.adapter_version !== env.adapter_version) {
    return { ok: false, reason: "version_mismatch" };
  }
  return { ok: true, adapter: ac.data };
}
