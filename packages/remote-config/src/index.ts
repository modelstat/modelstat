/**
 * `@modelstat/remote-config` — the generalized signed-config loader.
 *
 * The spine of the server-driven companion: fetch a signed bundle,
 * verify Ed25519 over the raw bytes against the bundled trust
 * root, cache it to disk, and fall back gracefully (memory → disk →
 * bundled) on any failure. One mechanism for every evolving config kind.
 *
 * This entry point is environment-agnostic (Node + browser). The Node
 * disk cache lives on the `./node` subpath so a browser bundle never
 * pulls in `node:fs`.
 */

export { base64ToBytes, bytesToBase64, sha256Hex } from "./crypto.js";
export * from "./loader.js";
export * from "./schema.js";
export * from "./types.js";
export * from "./verify.js";
