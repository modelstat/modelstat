/**
 * The generic signed-config wire contract.
 *
 * Generalizes the extension's `SignedAdapter` / `ManifestEntry`
 * (`@modelstat/adapters-protocol`) into a per-*kind* shape, so every
 * piece of evolving config (policies, prices, models, …) rides the same
 * signed delivery path instead of each one inventing its own.
 *
 * Trust comes from the Ed25519 signature over the RAW `config` bytes —
 * not from TLS. The `sha256` in the manifest is a cheap
 * transport-integrity cross-check, not the trust anchor.
 */

import { z } from "zod";

/** The signed envelope a server serves for one config kind + version. */
export const SignedBundle = z.object({
  /** Base64 of the config payload bytes that were signed. The verifier
   * checks the signature over these exact bytes BEFORE parsing them. */
  config: z.string().max(1_000_000),
  /** Base64 Ed25519 signature over the decoded `config` bytes. */
  signature: z.string().max(200),
  /** ISO-8601 time the publisher signed this bundle. */
  signed_at: z.string().datetime({ offset: true }),
  /** Monotonic version. Equals the payload's `version` and the manifest
   * entry's `version`; duplicated here for a parse-free comparison. */
  version: z.number().int().nonnegative(),
});
export type SignedBundle = z.infer<typeof SignedBundle>;

/** The cheap, frequently-polled pointer for one config kind. */
export const ConfigManifest = z.object({
  /** Echoes the requested kind — guards against a misrouted response. */
  kind: z.string().min(1).max(64),
  version: z.number().int().nonnegative(),
  /** Absolute, or api-relative, URL of the signed bundle for `version`. */
  bundle_url: z.string().min(1).max(400),
  /** Hex SHA-256 of the bundle response bytes. */
  sha256: z.string().length(64),
  signed_at: z.string().datetime({ offset: true }),
});
export type ConfigManifest = z.infer<typeof ConfigManifest>;

/**
 * Every config-kind payload must carry a monotonic `version`. The
 * verifier cross-checks payload.version === envelope.version ===
 * manifest.version so a stale bundle can't be replayed under a newer
 * manifest entry. Per-kind schemas extend this.
 */
export const VersionedConfig = z.object({
  version: z.number().int().nonnegative(),
});
export type VersionedConfig = z.infer<typeof VersionedConfig>;
