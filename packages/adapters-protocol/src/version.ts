/**
 * Wire-protocol version for the extension adapter format.
 *
 * Bump MAJOR when the interpreter's capability surface changes in a
 * backwards-incompatible way (new extractor kinds, removed fields,
 * changed merge semantics). The extension only executes configs whose
 * `protocol_version` is recognised by its bundled interpreter; unknown
 * versions trigger fallback to the last-known-good bundled adapter.
 *
 * Bump MINOR for purely additive extractor kinds — old interpreters
 * degrade gracefully (unknown extractor = skip, try next variant).
 */
export const ADAPTER_PROTOCOL_VERSION = 1 as const;
export type AdapterProtocolVersion = typeof ADAPTER_PROTOCOL_VERSION;
