/**
 * @modelstat/agent — privacy-first session sanitisation SDK.
 *
 * Use the library form when you have a session payload in memory and
 * want to redact / compact / pipe before uploading; use the bundled
 * `modelstat-daemon` CLI for stream-based pipelines on disk.
 *
 *   import { redact, compact, pipe } from "@modelstat/agent";
 *
 *   const { data: cleaned } = redact(session, "strict-pii-v2");
 *   const { data: small } = compact(cleaned, { maxToolOutput: 4096 });
 *   await fetch("/v1/ingest", { body: JSON.stringify(small), ... });
 */
export {
  redact,
  processingFor,
  POLICY_VERSIONS,
  type PolicyName,
  type RedactionResult,
} from "./redact.js";

export {
  compact,
  DEFAULT_COMPACT,
  type CompactOptions,
  type CompactResult,
} from "./compact.js";

import { redact, type PolicyName } from "./redact.js";
import { compact, type CompactOptions } from "./compact.js";

/**
 * One-shot pipeline: redact then compact. Returns the cleaned payload
 * + a fully-formed `processing` provenance block ready to attach to
 * the upload body.
 */
export function pipe<T>(
  input: T,
  opts: {
    policy?: PolicyName;
    compact?: Partial<CompactOptions>;
    agentId?: string;
  } = {},
): {
  data: T;
  processing: {
    redacted_by: string;
    redaction_policy: string;
    redaction_policy_version: string;
    redactions_applied: number;
    compacted: boolean;
    bytes_saved: number;
    changes_applied: number;
    original_size_bytes: number;
    uploaded_size_bytes: number;
  };
} {
  const policy = opts.policy ?? "strict-pii-v2";
  const agentId = opts.agentId ?? "modelstat-daemon-sdk";
  const before = JSON.stringify(input).length;

  const r = redact(input, policy);
  const c = compact(r.data, opts.compact ?? {});
  const after = JSON.stringify(c.data).length;

  return {
    data: c.data,
    processing: {
      redacted_by: agentId,
      redaction_policy: r.policy,
      redaction_policy_version: r.policyVersion,
      redactions_applied: r.redactionsApplied,
      compacted: c.changesApplied > 0,
      bytes_saved: c.bytesSaved,
      changes_applied: c.changesApplied,
      original_size_bytes: before,
      uploaded_size_bytes: after,
    },
  };
}
