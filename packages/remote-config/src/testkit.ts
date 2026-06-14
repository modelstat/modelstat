/**
 * TEST-ONLY signing helpers.
 *
 * Production clients only ever VERIFY — signing lives on the server, with
 * the private key. These helpers let tests mint valid signed bundles +
 * manifests with an ephemeral key, and underpin the cross-repo golden
 * vector. Never import this from shipping client code (it is exposed only
 * on the `./testkit` subpath, never re-exported from the package root).
 */

import { asBuf, bytesToBase64, sha256Hex, utf8ToBytes } from "./crypto.js";
import type { ConfigManifest, SignedBundle } from "./schema.js";

export interface TestSigner {
  /** Raw 32-byte Ed25519 public key. */
  publicKey: Uint8Array;
  sign(message: Uint8Array): Promise<Uint8Array>;
}

/** A fresh ephemeral Ed25519 keypair for round-trip tests. */
export async function generateTestKeypair(): Promise<TestSigner> {
  const pair = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, [
    "sign",
    "verify",
  ])) as CryptoKeyPair;
  const rawPub = new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey));
  return {
    publicKey: rawPub,
    async sign(message: Uint8Array): Promise<Uint8Array> {
      return new Uint8Array(
        await crypto.subtle.sign({ name: "Ed25519" }, pair.privateKey, asBuf(message)),
      );
    },
  };
}

export interface ServedBundle {
  bundle: SignedBundle;
  /** The exact bytes a server returns for this bundle — what the loader
   * hashes for the manifest sha256 cross-check. */
  bytes: string;
}

/** Mint a signed bundle for a payload carrying a numeric `version`. */
export async function signBundle(
  payload: { version: number } & Record<string, unknown>,
  signer: TestSigner,
  signedAt = "2026-06-14T00:00:00.000Z",
): Promise<ServedBundle> {
  const configBytes = utf8ToBytes(JSON.stringify(payload));
  const signature = await signer.sign(configBytes);
  const bundle: SignedBundle = {
    config: bytesToBase64(configBytes),
    signature: bytesToBase64(signature),
    signed_at: signedAt,
    version: payload.version,
  };
  return { bundle, bytes: JSON.stringify(bundle) };
}

/** Build the manifest pointer for a served bundle, with a correct sha256. */
export async function manifestFor(
  kind: string,
  served: ServedBundle,
  bundleUrl: string,
): Promise<ConfigManifest> {
  return {
    kind,
    version: served.bundle.version,
    bundle_url: bundleUrl,
    sha256: await sha256Hex(utf8ToBytes(served.bytes)),
    signed_at: served.bundle.signed_at,
  };
}
