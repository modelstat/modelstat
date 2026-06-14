/**
 * Cursor parser (skeleton).
 *
 * Cursor doesn't expose token counts — only AI-attributed code lines
 * (ai_code_hashes, scored_commits) and conversation metadata. v1 produces
 * *sessions* with message counts and empty token fields; the UI can still
 * show the session list and per-project activity. Token/cost estimation
 * from subscription cost ÷ events is a later feature.
 *
 * Uses sql.js (pure-WASM SQLite) rather than better-sqlite3 to keep the
 * install tree free of native bindings. The Cursor workspaceStorage .db
 * files are a few MB at most, so in-memory WASM reads are plenty fast.
 * No more deprecated prebuild-install in the npm warnings.
 */
import { readFileSync } from "node:fs";
import initSqlJs, { type Database, type SqlJsStatic } from "sql.js";
import type { RawEvent } from "@modelstat/core";
import { sourceEventId } from "@modelstat/core";
import type { ParseResult, ParserContext } from "../types.js";

interface AiCodeHashRow {
  hash: string;
  source: string | null;
  model: string | null;
  requestId: string | null;
  conversationId: string | null;
  timestamp: number | null;
}

let sqlJsPromise: Promise<SqlJsStatic> | null = null;

/**
 * Lazy-load the sql.js runtime. We only initialise once per process
 * regardless of how many Cursor .db files we parse — sql.js's wasm
 * module is about 650 KB and decoding it on every call would dominate
 * parse time.
 */
function getSqlJs(): Promise<SqlJsStatic> {
  if (!sqlJsPromise) sqlJsPromise = initSqlJs();
  return sqlJsPromise;
}

/**
 * Read the Cursor tracking DB and emit one RawEvent per AI-attributed
 * code hash. Async because sql.js's init is async; callers upstream
 * (the worker ingest pipeline) already await parser results.
 */
export async function parseCursorTrackingDb(
  ctx: ParserContext,
): Promise<ParseResult> {
  const events: RawEvent[] = [];
  let rawLines = 0;

  const SQL = await getSqlJs();
  const fileBytes = readFileSync(ctx.sourceFile);
  let db: Database | null = null;
  try {
    db = new SQL.Database(fileBytes);
    const result = db.exec(
      `SELECT hash, source, model, requestId, conversationId, timestamp
         FROM ai_code_hashes
        WHERE conversationId IS NOT NULL AND timestamp IS NOT NULL
        ORDER BY timestamp ASC`,
    );
    // sql.js returns an array of result sets (one per semicolon-separated
    // statement). An empty array means the query returned zero rows —
    // not an error, just a DB with no AI activity yet.
    const rows = rowsFromExec(result);
    for (const r of rows as unknown as AiCodeHashRow[]) {
      rawLines += 1;
      if (!r.conversationId || !r.timestamp) continue;
      const ts = new Date(r.timestamp).toISOString();
      events.push({
        source_event_id: sourceEventId(
          ctx.deviceId,
          `${ctx.sourceFile}#${r.hash}`,
          r.timestamp,
        ),
        ts,
        kind: "assistant_message",
        agent: "cursor",
        provider: "cursor",
        model: r.model,
        session_id: r.conversationId,
        turn_index: null,
        parent_event_id: null,
        cwd: null,
        git: null,
        tokens: null, // unknown — Cursor doesn't record
        duration_ms: null,
        tool_calls: {},
        files_touched: [],
        source_file: ctx.sourceFile,
        source_byte_offset: null,
      });
    }
  } finally {
    db?.close();
  }

  return {
    events,
    toolCalls: [], // ai_code_hashes has no tool-call data — always empty
    stats: {
      rawLines,
      emittedEvents: events.length,
      skipped: rawLines - events.length,
    },
    sourceFile: ctx.sourceFile,
  };
}

/**
 * sql.js returns `{columns, values}` where values is a 2D array. Turn
 * it into an array of {col: val} records keyed by column name so the
 * caller can read them like better-sqlite3's .all() output.
 */
function rowsFromExec(
  result: Array<{ columns: string[]; values: Array<Array<unknown>> }>,
): Array<Record<string, unknown>> {
  if (result.length === 0) return [];
  const first = result[0]!;
  const out: Array<Record<string, unknown>> = [];
  for (const row of first.values) {
    const rec: Record<string, unknown> = {};
    for (let i = 0; i < first.columns.length; i++) {
      rec[first.columns[i]!] = row[i];
    }
    out.push(rec);
  }
  return out;
}
