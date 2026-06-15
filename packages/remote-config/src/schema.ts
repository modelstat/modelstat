/**
 * The config wire contract.
 *
 * Each kind is served at `GET /v1/config/{kind}` as a plain JSON payload
 * carrying a monotonic `version`. Trust is the TLS connection to the
 * modelstat origin; the client validates the payload's shape (a per-kind
 * Zod schema) and version-gates against what it already holds. Per-kind
 * payload schemas extend `VersionedConfig`.
 */

import { z } from "zod";

export const VersionedConfig = z.object({
  version: z.number().int().nonnegative(),
});
export type VersionedConfig = z.infer<typeof VersionedConfig>;
