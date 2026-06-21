/**
 * The SDK's own version, baked in at build time (mirrors the Rust reference's
 * `env!("CARGO_PKG_VERSION")`). Kept in sync with `package.json`'s `version`.
 *
 * Ships as the wire `daemon_version` field, prefixed `node-sdk/`. The combined
 * string must stay ≤40 characters (the server's `daemon_version` cap).
 */
export const PKG_VERSION = "0.0.3";

/** The producer version string sent as `daemon_version`, e.g. `node-sdk/0.0.3`. */
export const CLIENT_VERSION = `node-sdk/${PKG_VERSION}`;
