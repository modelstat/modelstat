/**
 * Low-level crypto + encoding primitives, shared *verbatim* across Node
 * (the long-lived CLI/tray daemon) and the browser (the extension).
 * Everything here runs on the WebCrypto `crypto.subtle` surface that both
 * runtimes expose, plus `atob`/`btoa` (global in Node ≥16 and browsers).
 *
 * Hard rule: no Node-only or browser-only imports in this file. That is
 * what lets the verify path — the actual trust check — be the same code
 * on every client.
 */

const ED25519 = { name: "Ed25519" } as const;

/**
 * Coerce to an ArrayBuffer-backed view — the shape WebCrypto's
 * `BufferSource` requires (it rejects the wider `ArrayBufferLike` that
 * `Uint8Array` now defaults to). Every byte array in this package is
 * already ArrayBuffer-backed (allocated by `base64ToBytes`, `TextEncoder`,
 * or `Response.arrayBuffer`), so this is a type narrowing, not a copy.
 */
export function asBuf(b: Uint8Array): Uint8Array<ArrayBuffer> {
  return b as Uint8Array<ArrayBuffer>;
}

export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function bytesToBase64(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]!);
  return btoa(bin);
}

export function utf8ToBytes(s: string): Uint8Array {
  return new TextEncoder().encode(s);
}

export function bytesToUtf8(b: Uint8Array): string {
  return new TextDecoder().decode(b);
}

/**
 * Hex SHA-256 of raw bytes. Used to cross-check a fetched bundle against
 * the `sha256` in its manifest — a transport-integrity check only; the
 * Ed25519 signature is the trust anchor.
 */
export async function sha256Hex(bytes: Uint8Array): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", asBuf(bytes));
  const view = new Uint8Array(digest);
  let hex = "";
  for (let i = 0; i < view.length; i++) hex += view[i]!.toString(16).padStart(2, "0");
  return hex;
}

/** Import a raw 32-byte Ed25519 public key for verification. */
export async function importEd25519PublicKey(raw: Uint8Array): Promise<CryptoKey> {
  if (raw.length !== 32) {
    throw new Error(`ed25519 public key must be 32 bytes, got ${raw.length}`);
  }
  return crypto.subtle.importKey("raw", asBuf(raw), ED25519, false, ["verify"]);
}

/**
 * Verify an Ed25519 signature over `message`. Returns false (never
 * throws) on any verification error, so callers can treat a bad
 * signature and a thrown WebCrypto error identically: reject + fall back.
 */
export async function verifyEd25519(
  key: CryptoKey,
  signature: Uint8Array,
  message: Uint8Array,
): Promise<boolean> {
  try {
    return await crypto.subtle.verify(ED25519, key, asBuf(signature), asBuf(message));
  } catch {
    return false;
  }
}
