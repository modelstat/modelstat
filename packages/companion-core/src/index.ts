/**
 * @modelstat/companion-core — shared core for every companion runtime.
 *
 * Design: docs/companion-unification.md. One contract per concern
 * (IDs, logger, HTTP, queue state machine, pipeline, heartbeat);
 * runtime-specific adapters live in subpaths (node/, browser/) that
 * this package will gain as each unification step lands.
 *
 * Consumers should import from the subpaths (./contracts, ./http,
 * ./queue, ./pipeline, ./logger, ./ids, ./config) rather than this
 * barrel to keep bundle size tight in the extension.
 */

export * from "./contracts/index.js";
export * from "./ids.js";
export * from "./logger.js";
export * from "./config/index.js";
export * from "./http/index.js";
export * from "./queue/index.js";
export * from "./pipeline/index.js";
