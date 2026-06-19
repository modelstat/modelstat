/**
 * `@modelstat/remote-config` — the server-driven config loader.
 *
 * The spine of the server-driven daemon: fetch a config payload from the
 * modelstat origin over TLS, validate its shape, cache it to disk, and fall
 * back gracefully (memory → disk → bundled) on any failure. One mechanism
 * for every evolving config kind.
 *
 * This entry point is environment-agnostic (Node + browser). The Node disk
 * cache lives on the `./node` subpath so a browser bundle never pulls in
 * `node:fs`.
 */

export * from "./loader.js";
export * from "./schema.js";
export * from "./types.js";
