/**
 * Shared logger contract for companions.
 *
 * Runtime-agnostic interface with a default `consoleLogger` implementation
 * that works in both Node (stdout) and the browser (devtools). Node can
 * swap in pino via `createNodeLogger()` from
 * `@modelstat/companion-core/node/logger-pino` (declared in the companion-
 * unification doc; implementation arrives when the CLI is rewired).
 */

export type LogLevel = "debug" | "info" | "warn" | "error";

export interface Logger {
  debug(msg: string, fields?: Record<string, unknown>): void;
  info(msg: string, fields?: Record<string, unknown>): void;
  warn(msg: string, fields?: Record<string, unknown>): void;
  error(msg: string, fields?: Record<string, unknown> | Error): void;
  child(scope: string): Logger;
}

const LEVEL_RANK: Record<LogLevel, number> = {
  debug: 10,
  info: 20,
  warn: 30,
  error: 40,
};

/**
 * Resolve the default log level from env. Node reads `LOG_LEVEL`;
 * extensions override via the options page. Falls back to "info" in
 * production (NODE_ENV=production) and "debug" everywhere else.
 */
export function defaultLevel(): LogLevel {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const env = (globalThis as any).process?.env ?? {};
  const raw = (env.LOG_LEVEL ?? "").toLowerCase();
  if (raw === "debug" || raw === "info" || raw === "warn" || raw === "error") return raw;
  return env.NODE_ENV === "production" ? "info" : "debug";
}

export interface ConsoleLoggerOpts {
  scope?: string;
  level?: LogLevel;
}

/**
 * Default logger — emits scoped, level-gated messages via console.* with
 * a `[modelstat:scope]` prefix. Lifted from
 * apps/extension/src/common/logger.ts and generalised; works unchanged
 * in Node.
 */
export function consoleLogger(opts: ConsoleLoggerOpts = {}): Logger {
  const scope = opts.scope ?? "main";
  const level = opts.level ?? defaultLevel();
  const threshold = LEVEL_RANK[level];
  const emit = (lvl: LogLevel, msg: string, fields?: unknown): void => {
    if (LEVEL_RANK[lvl] < threshold) return;
    const prefix = `[modelstat:${scope}]`;
    const args = fields !== undefined ? [prefix, msg, fields] : [prefix, msg];
    const fn: "log" | "warn" | "error" =
      lvl === "warn" ? "warn" : lvl === "error" ? "error" : "log";
    // biome-ignore lint/suspicious/noConsole: intentional
    console[fn](...args);
  };
  return {
    debug: (msg, fields) => emit("debug", msg, fields),
    info: (msg, fields) => emit("info", msg, fields),
    warn: (msg, fields) => emit("warn", msg, fields),
    error: (msg, fields) => emit("error", msg, fields),
    child: (childScope: string) =>
      consoleLogger({ scope: `${scope}:${childScope}`, level }),
  };
}

/**
 * Standard factory — companions should call this rather than constructing
 * loggers directly, so we can swap in a pino-backed impl in Node without
 * touching call sites.
 */
export function createLogger(scope: string, opts: { level?: LogLevel } = {}): Logger {
  return consoleLogger({ scope, ...opts });
}

/**
 * Unwrap an Error's `cause` chain into a single-line summary. Undici's
 * global fetch throws `TypeError: fetch failed` with the real cause
 * buried in `.cause` (and sometimes `.cause.errors[0]` for
 * AggregateErrors). Without this the daemon just shows "fetch failed"
 * to the user, which is useless for diagnosing ECONNREFUSED /
 * ENOTFOUND / cert issues / etc.
 *
 * Output shape: "OuterMsg → code=CODE msg=InnerMsg [→ …]"
 * Bounded to a few levels to avoid runaway chains.
 */
export function describeErrorWithCause(err: unknown, depth = 4): string {
  if (!err) return "unknown";
  if (!(err instanceof Error)) return String(err);
  const parts: string[] = [err.message || err.name];
  let cur: unknown = (err as { cause?: unknown }).cause;
  let left = depth;
  while (cur && left-- > 0) {
    if (cur instanceof AggregateError && Array.isArray(cur.errors) && cur.errors[0]) {
      cur = cur.errors[0];
      continue;
    }
    if (cur instanceof Error) {
      const code = (cur as NodeJS.ErrnoException).code;
      parts.push(`${code ? `code=${code} ` : ""}msg=${cur.message || cur.name}`);
      cur = (cur as { cause?: unknown }).cause;
      continue;
    }
    parts.push(String(cur));
    break;
  }
  return parts.join(" → ");
}
