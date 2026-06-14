/**
 * Scoped logger. In dev, everything above debug prints; in prod, only
 * warn+error. Every message is prefixed with its scope so filtering in
 * DevTools is trivial.
 */

type Level = "debug" | "info" | "warn" | "error";

const LEVEL_ORDER: Record<Level, number> = { debug: 0, info: 1, warn: 2, error: 3 };
const MIN_LEVEL: Level = import.meta.env.DEV ? "debug" : "warn";

export interface Logger {
  debug(...args: unknown[]): void;
  info(...args: unknown[]): void;
  warn(...args: unknown[]): void;
  error(...args: unknown[]): void;
}

export function createLogger(scope: string): Logger {
  const emit = (level: Level, args: unknown[]) => {
    if (LEVEL_ORDER[level] < LEVEL_ORDER[MIN_LEVEL]) return;
    const fn = console[level] ?? console.log;
    fn(`[modelstat:${scope}]`, ...args);
  };
  return {
    debug: (...a) => emit("debug", a),
    info: (...a) => emit("info", a),
    warn: (...a) => emit("warn", a),
    error: (...a) => emit("error", a),
  };
}
