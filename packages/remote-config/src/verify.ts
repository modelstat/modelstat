/**
 * Signature verification — the trust check at the heart of the loader.
 *
 * Ported from the extension's `interpreter/signature.ts`, generalized to
 * any kind's payload schema. The shape that matters for security is
 * unchanged: verify the Ed25519 signature over the RAW config bytes
 * BEFORE `JSON.parse`, so the parser only ever sees bytes a holder of the
 * private key produced. A compromised server (or a tampered disk cache)
 * cannot forge a bundle — verification fails and the caller falls back to
 * last-known-good.
 */

import type { ZodType } from "zod";
import { base64ToBytes, bytesToUtf8, importEd25519PublicKey, verifyEd25519 } from "./crypto.js";
import { SignedBundle } from "./schema.js";

export type VerifyFailure =
  | "envelope_invalid"
  | "pubkey_invalid"
  | "signature_mismatch"
  | "config_not_json"
  | "payload_schema_invalid"
  | "version_mismatch";

export type VerifyResult<T> =
  | { ok: true; value: T; version: number }
  | { ok: false; reason: VerifyFailure };

/**
 * Verify already-decoded config bytes against a signature. The core
 * primitive — used for freshly-fetched bundles and for re-verifying the
 * disk cache on read (so a cache an attacker tampered with on disk is
 * rejected and the loader falls back rather than trusting it).
 */
export async function verifyConfigBytes<T extends { version: number }>(args: {
  configBytes: Uint8Array;
  signature: Uint8Array;
  publicKey: Uint8Array;
  schema: ZodType<T>;
  expectedVersion: number;
}): Promise<VerifyResult<T>> {
  let key: CryptoKey;
  try {
    key = await importEd25519PublicKey(args.publicKey);
  } catch {
    return { ok: false, reason: "pubkey_invalid" };
  }

  const valid = await verifyEd25519(key, args.signature, args.configBytes);
  if (!valid) return { ok: false, reason: "signature_mismatch" };

  // Only now — past the signature — do we parse. The input is trusted.
  let parsed: unknown;
  try {
    parsed = JSON.parse(bytesToUtf8(args.configBytes));
  } catch {
    return { ok: false, reason: "config_not_json" };
  }

  const result = args.schema.safeParse(parsed);
  if (!result.success) return { ok: false, reason: "payload_schema_invalid" };

  // payload.version must equal the version the envelope/manifest claimed,
  // closing a replay where an old signed bundle is served under a new tag.
  if (result.data.version !== args.expectedVersion) {
    return { ok: false, reason: "version_mismatch" };
  }
  return { ok: true, value: result.data, version: args.expectedVersion };
}

/** Verify a full `SignedBundle` envelope (the freshly-fetched form). */
export async function verifySignedBundle<T extends { version: number }>(args: {
  envelope: unknown;
  publicKey: Uint8Array;
  schema: ZodType<T>;
}): Promise<VerifyResult<T>> {
  const env = SignedBundle.safeParse(args.envelope);
  if (!env.success) return { ok: false, reason: "envelope_invalid" };
  return verifyConfigBytes({
    configBytes: base64ToBytes(env.data.config),
    signature: base64ToBytes(env.data.signature),
    publicKey: args.publicKey,
    schema: args.schema,
    expectedVersion: env.data.version,
  });
}
