/**
 * The `policies` config kind: a signed, ADDITIVE redaction augment layered
 * over the local privacy floor (`./redact-floor`).
 *
 * Hard invariant, enforced by construction: a bundle can only ADD secret
 * patterns. There is no field that removes, disables, or replaces the floor —
 * the worst a (validly signed) bundle can do is cause *more* redaction. The
 * floor itself is compiled into the binary and applied unconditionally
 * (`./redact`). See the never-weakenable tests.
 *
 * This module is the one that may use zod — it is consumed by the daemon's
 * signed-config loader, never by the published SDK (which
 * imports only the dependency-free `./redact-floor`).
 */

import { z } from "zod";
import type { RemoteRedactionPattern } from "./redact.js";

export const RedactionPattern = z.object({
  /** Stable label for the `[REDACTED:name]` placeholder. */
  name: z
    .string()
    .min(1)
    .max(64)
    .regex(/^[a-z0-9_]+$/, "name must be lowercase a-z0-9_"),
  /** JS regex source. Compiled with the `g` flag on the client. */
  regex: z.string().min(1).max(1000),
  /** Optional extra flags; `g` is always added. Limited to `i m s u`. */
  flags: z
    .string()
    .max(8)
    .regex(/^[imsu]*$/, "only i m s u flags allowed")
    .optional(),
});
export type RedactionPattern = z.infer<typeof RedactionPattern>;

export const RedactionPolicyBundle = z.object({
  version: z.number().int().nonnegative(),
  /** Additive patterns unioned ON TOP of the bundled floor. There is, by
   * design, no way to express removal — only addition. */
  patterns: z.array(RedactionPattern).max(256),
});
export type RedactionPolicyBundle = z.infer<typeof RedactionPolicyBundle>;

/** The bundled fallback: no augment. The floor alone applies until a signed
 * bundle is verified. */
export const POLICIES_BUNDLED_FALLBACK: RedactionPolicyBundle = { version: 0, patterns: [] };

/**
 * Config-kind descriptor for `@modelstat/remote-config`'s loader. Typed
 * structurally so `@modelstat/core` need not depend on the loader package.
 */
export const POLICIES_CONFIG_KIND: {
  readonly kind: "policies";
  readonly schema: typeof RedactionPolicyBundle;
  readonly bundledFallback: RedactionPolicyBundle;
} = {
  kind: "policies",
  schema: RedactionPolicyBundle,
  bundledFallback: POLICIES_BUNDLED_FALLBACK,
};

/**
 * Compile a verified bundle's patterns into runnable redaction patterns.
 * Invalid regexes are skipped, never thrown — a bad remote pattern must not
 * take down redaction. The `g` flag is always present so `String.replace`
 * redacts every occurrence.
 */
export function compilePolicyPatterns(bundle: RedactionPolicyBundle): RemoteRedactionPattern[] {
  const out: RemoteRedactionPattern[] = [];
  for (const p of bundle.patterns) {
    const flags = `g${p.flags ?? ""}`;
    try {
      out.push({ name: p.name, pattern: new RegExp(p.regex, flags) });
    } catch {
      // skip an un-compilable pattern; the floor still applies regardless
    }
  }
  return out;
}
