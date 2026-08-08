import { z } from "zod";
import {
  AGENTS,
  CLASSIFICATION_CONFIDENCE,
  DAEMON_PHASES,
  EVENT_KINDS,
  IDENTITY_OWNER_SCOPES,
  INSTALL_METHODS,
  OS_FAMILIES,
  PROVIDERS,
  TOOL_CALL_STATUSES,
} from "./enums.js";
import { EventReferences, SessionMetadata } from "./session-metadata.js";

/**
 * Canonical event — one row per meaningful turn inside a session.
 * Every parser (Claude Code JSONL, Codex rollout, Cursor SQLite, …)
 * normalises into this shape. The ingest endpoint only accepts this shape.
 */

export const TokenUsage = z.object({
  input: z.number().int().nonnegative().default(0),
  output: z.number().int().nonnegative().default(0),
  cache_creation: z.number().int().nonnegative().default(0),
  cache_read: z.number().int().nonnegative().default(0),
  /** Present on reasoning-model turns (Codex gpt-5.4, Claude extended thinking). */
  reasoning: z.number().int().nonnegative().default(0),
});
export type TokenUsage = z.infer<typeof TokenUsage>;

/** Git context derived from cwd → nearest `.git`. */
export const GitContext = z.object({
  remote_url: z.string().nullable(),
  /** The forge host, and only when git itself named it (parsed out of
   * `remote.origin.url`). Null everywhere else — a slug read off a directory
   * layout says nothing about where the repo is hosted. */
  remote_host: z.string().nullable(), // "github.com"
  remote_slug: z.string().nullable(), // "org/repo"
  branch: z.string().nullable(),
  /** How `remote_slug` was reached, so a fact and a guess are distinguishable:
   * `git_remote` (the repo's configured remote — the only source that can also
   * name a host), `repo_root_dir` (a real repo with no remote, keyed on its
   * root directory name), `path_shape` (inferred from the cwd's shape, no repo
   * reachable). Absent when there is no slug, and from daemons predating it. */
  slug_source: z.string().max(40).optional(),
});
export type GitContext = z.infer<typeof GitContext>;

/** One raw event from the agent. `source_event_id` is the dedupe key. */
export const RawEvent = z.object({
  source_event_id: z.string(),
  ts: z.string().datetime({ offset: true }),
  /** The turn's category. `EVENT_KINDS` is the vocabulary the daemon MEANS to
   * emit, not the set of values that can arrive: a parser that meets a record
   * type it has no arm for reports the type VERBATIM here rather than dropping
   * the record, which is how an upstream schema move becomes visible instead of
   * silent. Validated as a string for the same reason `modelstat-wire` does —
   * shape is the contract, membership is a question for the reader. */
  kind: z.string().max(120),

  // Attribution
  /** `AGENTS` is the roster of tools we have SEEN, and new ones ship weekly.
   * Discovery finds a tool by artefact shape rather than by name, so a value
   * here can legitimately be one no build of this package has heard of. */
  agent: z.string().max(120),
  /** `PROVIDERS` likewise. A multi-vendor harness (pi) names whatever vendor its
   * own config names, and folding an unlisted one to `unknown` does not merely
   * lose detail — it breaks the join to the account that paid for the tokens. */
  provider: z.string().max(120),
  model: z.string().max(120).nullable(),
  session_id: z.string().max(120), // agent-local session id (UUID in most cases)
  turn_index: z.number().int().nonnegative().nullable(),
  parent_event_id: z.string().nullable(), // for subagent turns

  // Location
  cwd: z.string().nullable(),
  git: GitContext.nullable(),

  // Resource usage
  tokens: TokenUsage.nullable(),
  /** Counters the source stated that do not map onto TokenUsage's five buckets,
   * keyed by their path in the source object with the source's own names.
   * Absent in the ordinary case; it fills when an upstream moves its token
   * schema, so the numbers survive to be re-bucketed instead of being lost
   * behind a fabricated zero. Numeric leaves only — a drifted shape is one
   * nothing has validated, and only its numbers are safe to carry.
   *
   * `.optional()` rather than `.default({})`: the daemon omits the key entirely
   * when there is nothing to say, so an `{}` in the type would describe a value
   * the wire never carries. */
  tokens_unmapped: z.record(z.string(), z.number().int().nonnegative()).optional(),
  duration_ms: z.number().int().nonnegative().nullable(),

  // Tool calls (aggregate only)
  tool_calls: z.record(z.string(), z.number().int().nonnegative()).default({}),

  // Files touched, relative to git root. Never absolute — scrubbed by agent.
  files_touched: z.array(z.string().max(512)).max(256).default([]),

  // Redacted excerpt of the conversation turn (user prompt or
  // assistant response). The PARSER is responsible for:
  //   1. Pulling a representative snippet from the turn (≤320 chars).
  //   2. Running it through @modelstat/core/redact PLUS, when
  //      available, the on-device Privacy Filter adapter.
  //   3. Stripping code blocks and file-path noise.
  // Optional — events without it fall back to metadata-only abstracts
  // (the historical behaviour). The daemon-core pipeline runs
  // redact() over it again as defence-in-depth before building the
  // summarize prompt; it never gets stored long-term server-side, only
  // used to construct the summarize input.
  content_excerpt: z.string().max(320).optional(),

  // Public code references (PRs, issues, repos) detected on-device
  // from this turn's FULL text — the high-recall feed the server rolls up into
  // SessionMetadata. Only public reference shapes (forge URLs, slugs, numbers,
  // ticket keys) ride here, never raw text — so it is derived pre-redaction
  // safely (same class as git.remote_slug). Optional + additive.
  references: EventReferences.optional(),

  // Reference to originating file for reparsing
  source_file: z.string().max(1024).nullable(),
  source_byte_offset: z.number().int().nonnegative().nullable(),
});
export type RawEvent = z.infer<typeof RawEvent>;

/** Redaction report summary — counts only, never actual content.
 *
 * Three guaranteed fields cover the regex pass (secrets / emails /
 * absolute-paths). Additional `pf_<category>` keys appear when the
 * daemon runs the OpenAI Privacy Filter model client-side — one
 * counter per detected category (pf_name, pf_address, pf_email, etc.).
 * `.catchall()` keeps them on the parsed object instead of stripping. */
export const RedactionReport = z
  .object({
    secrets_found: z.number().int().nonnegative().default(0),
    emails_redacted: z.number().int().nonnegative().default(0),
    paths_redacted_absolute: z.number().int().nonnegative().default(0),
  })
  .catchall(z.number().int().nonnegative());
export type RedactionReport = z.infer<typeof RedactionReport>;

/** Daemon-tagged segment — the unit of sync between daemon and server.
 *
 * A segment is a semantically-coherent slice of a session: its own tokens,
 * its own tags, its own redacted abstract. The daemon produces segments
 * by redact → tokenize → segment → summarise → tag on device; the server
 * never sees unredacted text.
 *
 * `segment_id` is deterministic (sha256 of session_id ‖ start ‖ end ‖
 * sorted(source_event_ids)), so re-running the pipeline on the same events
 * reproduces the same id and upload is idempotent at the segment level. */
/**
 * A daemon-emitted tag hint. `root_key` + `name` together identify a
 * target taxonomy node inside the owning org. Root keys are NOT a fixed
 * enum — each org's taxonomy tree can have any set of roots; the
 * TAXONOMY_ROOTS constant in @modelstat/core/enums is just the seed
 * list inserted into a freshly-created org.
 *
 * The server may re-derive its own classification from these hints; the
 * raw hint list also lives on the segment as a debuggable audit trail.
 */
export const TaxonomyHintRooted = z.object({
  root_key: z.string().max(60),
  name: z.string().max(120),
  confidence: z.number().min(0).max(1).default(0.7),
  /** Optional free-text reason the daemon attached this tag — surfaces
   * in the audit log so the user can see "why was this tagged X?" */
  reason: z.string().max(200).optional(),
});
export type TaxonomyHintRooted = z.infer<typeof TaxonomyHintRooted>;

export const Segment = z.object({
  /** sha256-based deterministic id — see @modelstat/core/ids.ts segmentId(). */
  segment_id: z.string().max(64),
  session_id: z.string().max(120),
  /** Open set — see `RawEvent.agent`. */
  agent: z.string().max(120),
  started_at: z.string().datetime({ offset: true }),
  ended_at: z.string().datetime({ offset: true }),
  /** Pre-redacted abstract, ≤ 512 chars. Never contains PII. */
  abstract: z.string().max(512),
  /** Tokens spent inside this segment only. */
  tokens: TokenUsage,
  /** Tags with strongly-typed root keys. The server may merge these with
   * its own classifier output. */
  tags: z.array(TaxonomyHintRooted).max(40).default([]),
  /** Counts of what was stripped. */
  redaction: RedactionReport,
  /** `source_event_id`s covered by this segment. Used for dedupe + replay. */
  source_event_ids: z.array(z.string()).max(2000),
  /** Optional embedding of the abstract (BGE-small-en-v1.5, 384 dims).
   * Present when the daemon has an Embedder adapter configured. */
  abstract_embedding: z.array(z.number()).length(384).optional(),
  /** Privacy-preserving on-device behavioral signal — COUNTS/RATIOS ONLY,
   * never raw text (mirrors RedactionReport). Powers server-side prompt-
   * friction detection. Optional so older daemons that omit it still validate. */
  behavior: z
    .object({
      /** Developer messages in this segment. */
      user_turns: z.number().int().nonnegative().default(0),
      /** User messages that land right after the assistant — a re-prompt /
       * correction proxy. */
      correction_count: z.number().int().nonnegative().default(0),
      /** 0-1 frustration estimate. The daemon NO LONGER PRODUCES this: it was
       * `max(correction_count / 4, 0.8 if a mood tag matched one of nine
       * English stems)` — hard-coded weights and a substring list scoring the
       * model's own free text, on a device that cannot revise either. It is
       * omitted rather than zeroed, so "no opinion" stays distinguishable from
       * "calm"; the counts above and the `[Mood: …]` tags are the inputs, and
       * scoring them is the server's job. Optional for payloads that predate
       * the removal. */
      frustration: z.number().min(0).max(1).optional(),
    })
    .optional(),
  /** Distilled "what the developer asked for / how they directed the AI" — from
   * their MESSAGES ONLY (not the assistant's actions), redacted, ≤512. The
   * source Insights' rule + skill detectors mine, distinct from the outcome
   * `abstract`. Optional; absent from daemons that predate it. */
  user_intent: z.string().max(512).optional(),
  /** The daemon machine's LOCAL wall clock at `started_at` — the one fact only
   * the device holds, since every timestamp on the wire is UTC. The
   * `time_of_day` / `cadence` tags are a CUT of this made on a machine that
   * cannot revise it; with the reading present the server can re-derive them
   * and cut differently. Optional; absent from daemons that predate it. */
  local_time: z
    .object({
      /** Minutes east of UTC at that instant (-420 for UTC-7), DST included. */
      utc_offset_minutes: z.number().int().min(-840).max(840),
      hour: z.number().int().min(0).max(23),
      /** 0=Sunday … 6=Saturday (JS `getDay()`). */
      weekday: z.number().int().min(0).max(6),
    })
    .optional(),
});
export type Segment = z.infer<typeof Segment>;

/**
 * On-device action decomposition of a tool call — a nested, additive object so
 * the top-level ToolCallWire stays stable as attributes grow. Bit-aligned with
 * the backend's Rust wire schema: the field set, caps, and defaults must match
 * exactly. Privacy: only governed tokens, the value-masked `param_shape`, and
 * the compliance-redacted command (PII/secrets stripped) ride this —
 * un-redacted raw never does. Produced on-device.
 */
export const ToolAction = z
  .object({
    /** Where it ran: `shell`, `mcp`, `builtin`, `browser`. (tier 0) */
    surface: z.string().max(40),
    /** Concrete program/operation, or a generic bucket token. (tier 0 | bucket) */
    executable: z.string().max(80).nullable().default(null),
    /** Verb/intent (`restart`, `read`, …). (tier 0) */
    action: z.string().max(40).nullable().default(null),
    /** What it acts on (`deployment`, `file`, …). (tier 0) */
    object: z.string().max(60).nullable().default(null),
    /** Governed safe flags (`destructive`, `remote`, …). (tier 0) */
    qualifiers: z.array(z.string().max(40)).max(8).default([]),
    /** Value-masked argument skeleton (every value → `§`). Carried in full up
     * to a malicious-size guard (mirrors backend `MAX_TOOL_ACTION_PARAM_SHAPE_CHARS`);
     * the daemon clamps rather than truncating semantically. (tier 1) */
    param_shape: z.string().max(16_384).nullable().default(null),
    /** Relevant non-sensitive keywords (e.g. ["rollout","restart","prod"]),
     * OpenAI-redacted on-device. (tier 0) */
    keywords: z.array(z.string().max(40)).max(12).default([]),
    /** Human-readable command summary (e.g. "redeploying service payments-api"),
     * OpenAI-redacted on-device. (tier 0) */
    abstract: z.string().max(200).nullable().default(null),
    /** The compliance-redacted command text — PII/secrets stripped on-device
     * (SOC2/GDPR), org-internal infra intact; the server derives semantics from
     * it. Un-redacted raw never ships. (tier 0, post-redaction) */
    command_redacted: z.string().max(16_384).nullable().default(null),
    /** Per-script content abstracts for any script/bash FILES the command runs
     * — summarized on-device by the local model, then redacted. Ordered by
     * appearance; `token` is the script's token exactly as it appears in
     * `command_redacted`, so the backend deterministically zips each `summary`
     * to its place when ingesting the command + its scripts. (tier 0) */
    scripts: z
      .array(z.object({ token: z.string().max(200), summary: z.string().max(200) }))
      .max(8)
      .default([]),
    /** Extractor confidence in [0, 1]. */
    confidence: z.number().min(0).max(1).default(0),
    /** Provenance of the extraction, e.g. `shell.v3`. */
    extractor: z.string().max(40),
  })
  .strict();
export type ToolAction = z.infer<typeof ToolAction>;

/**
 * One tool invocation made by an agent during a session — the per-call
 * daemon to RawEvent.tool_calls (which stays an aggregate count map).
 *
 * Naming discipline: `agent` is the AGENTS enum value (claude_code,
 * codex_cli, …) — the AI client that ran the call. `server`/`name`
 * describe the invoked capability (Bash, Read, mcp:github/create_pr).
 * Never conflate the two.
 *
 * PRIVACY CONTRACT: no raw tool arguments, results, file paths, or
 * command text ever ride this wire. The only payload-derived fields
 * are one-way hashes (`args_hash` / `signature_hash`), byte sizes
 * (`args_bytes` / `result_bytes`), and the governed, value-masked
 * `action` decomposition (ToolAction, tier ≤ 1) — governed tokens and
 * a `§`-masked param skeleton, never the command itself. Tool names
 * ship verbatim: they are vendor identifiers, not user content
 * (dynamic-looking hex/UUID tails are normalised to `<dyn>` at parse
 * time).
 */
export const ToolCallWire = z.object({
  /** tool_use block `id` / codex `call_id`; parsers fall back to a
   * deterministic `tc_<djb2-base36>` of `${source_event_id}|${call_index}`
   * when the source line carries no id. */
  external_call_id: z.string().max(120),
  /** Agent-local session id — same id space as RawEvent.session_id. */
  session_id: z.string().max(120),
  /** The RawEvent that contained the tool_use (dedupe/replay anchor). */
  source_event_id: z.string(),
  /** Segment containing source_event_id — filled by the daemon at
   * batch-build time when known, else null. */
  segment_id: z.string().max(64).nullable().default(null),
  /** The agent that made the call. Open set — see `RawEvent.agent`. */
  agent: z.string().max(120),
  /** `builtin` or `mcp:<server>`. */
  server: z.string().max(120),
  /** Bare tool name (`Bash`, `create_pr`) — normalised vendor identifier. */
  name: z.string().max(120),
  turn_index: z.number().int().nonnegative().nullable(),
  /** Ordinal of the call within its source event (0-based). */
  call_index: z.number().int().nonnegative(),
  /** ts of the line carrying the tool_use. */
  started_at: z.string().datetime({ offset: true }),
  /** ts of the line carrying the matching tool_result; null if unmatched. */
  ended_at: z.string().datetime({ offset: true }).nullable(),
  status: z.enum(TOOL_CALL_STATUSES),
  /** Hex sha256 of JSON.stringify(input); `""` when the call had no input. */
  args_hash: z.string().max(64),
  /** Sha256 of the sorted top-level arg key names joined by `,`; the
   * literal `none` when input is not a non-empty object. */
  signature_hash: z.string().max(64),
  /** UTF-8 byte length of JSON.stringify(input); 0 if none. */
  args_bytes: z.number().int().nonnegative(),
  /** UTF-8 byte length of JSON.stringify(tool_result content); 0 if
   * unmatched/empty. */
  result_bytes: z.number().int().nonnegative(),
  /** Model of the assistant message that issued the call. `<synthetic>`
   * kept verbatim per the PR #12 attribution rules. */
  model: z.string().max(120).nullable(),
  /** On-device action decomposition — nested + additive, `null` when nothing
   * was extracted. Replaces `command_families`. */
  action: ToolAction.nullable().default(null),
});
export type ToolCallWire = z.infer<typeof ToolCallWire>;

/** Bundle the daemon ships to the server in one request.
 *
 * The daemon runs the full pipeline locally
 * (redact → segment → summarise → tag) and ships the finished
 * `segments: Segment[]` alongside the raw `events`. Events are
 * internal plumbing (cost math, event-level drilldown);
 * segments are the analytics unit. */
export const IngestBatch = z.object({
  batch_id: z.string(), // ULID
  device_id: z.string(),
  daemon_version: z.string().max(40),
  events: z.array(RawEvent).max(10_000),
  segments: z.array(Segment).max(2_000).default([]),
  /** Per-call tool invocations (additive — old agents omit it, old
   * servers ignore it). See ToolCallWire for the privacy contract:
   * hashes / byte sizes / governed action tokens only, never payloads. */
  tool_calls: z.array(ToolCallWire).max(20_000).default([]),
  /** Optional per-session metadata hint: which installation produced them, etc. */
  session_installs: z
    .record(
      z.string(),
      z.object({
        installation_id: z.string(),
        identity_id: z.string().nullable(),
      }),
    )
    .optional(),
  /** Optional per-session titles — session_id → short redacted title
   * (≤120 chars) produced by the daemon's local titler from the
   * session's segment abstracts. Daemons recompute it from the full
   * session view on every upload, so the latest batch always carries the
   * freshest title. Absent for runtimes without a titler (older agents,
   * no-op browser summariser). */
  session_titles: z.record(z.string(), z.string().max(120)).optional(),
  /** Optional per-session deterministic metadata — session_id →
   * {@link SessionMetadata}: the repos, pull requests, and issues the
   * session touched, detected on-device across git context, tool calls,
   * redacted content, and the local model (so it works for any provider).
   * Additive — old daemons omit it, old servers ignore it (the wire has no
   * `deny_unknown_fields`). The join layer between AI spend and shipped work. */
  session_metadata: z.record(z.string(), SessionMetadata).optional(),
  /** Where this batch's session abstracts were produced: "local" (on-device
   * model), "self-hosted" (the org's own endpoint), or "cloud" (server-side
   * edge summariser via /v1/ingest/raw). Redaction stays client-side in every
   * mode — only the summarisation LOCATION differs. Additive — old daemons omit
   * it, old servers ignore it; the server records it as the scope's last-seen
   * mode for ops-alert enrichment. */
  summarizer_mode: z.enum(["local", "self-hosted", "cloud"]).optional(),
  /** Where this batch's layer-2 PII detection ran when it was BUILT: "local"
   * (on-device model), "cloud" (modelstat's /v1/redact classifier), or
   * "self-hosted" (the org's own endpoint). The layer-1 secret floor runs
   * on-device in every mode. Stamped at the spool door, not at upload, so a
   * batch that waited out a mode switch still names the mode that actually
   * scrubbed it. Additive — old daemons omit it, old servers ignore it. */
  redactor_mode: z.enum(["local", "self-hosted", "cloud"]).optional(),
});
export type IngestBatch = z.infer<typeof IngestBatch>;

/** Unified heartbeat payload emitted by every daemon. CLI and
 * extension populate all fields; fields that don't apply to a runtime
 * use sensible zeros / nulls rather than being omitted, so the server
 * can parse one schema. */
export const HeartbeatPayload = z.object({
  device_id: z.string(),
  status: z.enum(DAEMON_PHASES),
  message: z.string().max(240).nullable(),
  progress_done: z.number().int().nonnegative().default(0),
  progress_total: z.number().int().nonnegative().default(0),
  queue_size: z.number().int().nonnegative().default(0),
  stats: z.record(z.string(), z.unknown()).default({}),
  last_event_at: z.string().datetime({ offset: true }).nullable(),
  daemon_version: z.string().max(40),
});
export type HeartbeatPayload = z.infer<typeof HeartbeatPayload>;

/** Device enrollment payload. */
export const DeviceEnrollment = z.object({
  machine_id: z.string(), // stable, platform-provided
  hostname: z.string().max(120),
  os_family: z.enum(OS_FAMILIES),
  os_version: z.string().max(60),
  arch: z.enum(["x86_64", "arm64", "other"]),
  daemon_version: z.string().max(40),
});
export type DeviceEnrollment = z.infer<typeof DeviceEnrollment>;

/**
 * Self-register flow (POST /v1/devices/self-register). The daemon generates
 * an identity client-side (UUIDv7 + optional ed25519 keypair) and registers
 * itself without any prior auth. Server returns a one-time `device_secret`
 * the daemon uses for subsequent calls, plus a 3-word `claim_code` the
 * human will use to attach the device to their account.
 */
export const DeviceSelfRegister = z.object({
  /** Daemon-generated UUIDv7 — must pass shape + recent-timestamp checks. */
  device_uuid: z.string(),
  /** Base64-encoded ed25519 public key, exactly 32 raw bytes. Optional
   * but recommended (used for sender-constrained tokens / DPoP later). */
  public_key: z.string().optional(),
  /** Free-form snapshot shown to the human on the claim page so they
   * can sanity-check what they're claiming. */
  fingerprint: z
    .object({
      hostname: z.string().max(120).optional(),
      os: z.string().max(60).optional(),
      os_family: z.enum(OS_FAMILIES).optional(),
      os_version: z.string().max(60).optional(),
      arch: z.enum(["x86_64", "arm64", "other"]).optional(),
      daemon: z.string().max(80).optional(),
      daemon_version: z.string().max(40).optional(),
      // Allow extra fields for forward-compat without breaking old agents.
    })
    .catchall(z.union([z.string(), z.number(), z.boolean()]))
    .default({}),
});
export type DeviceSelfRegister = z.infer<typeof DeviceSelfRegister>;

export const DeviceClaimRequest = z.object({
  claim_code: z.string().max(40),
  /** Optional org_id if the user belongs to multiple orgs and wants to
   * claim into a specific one. Defaults to personal org. */
  org_id: z.string().uuid().optional(),
});
export type DeviceClaimRequest = z.infer<typeof DeviceClaimRequest>;

/**
 * Provenance for client-side sanitisation. Attached to ingest payloads so
 * the dashboard can show what was redacted/compacted, by which agent, with
 * which policy version. If absent on an upload, the service applies the
 * account's default policy and stamps its own metadata.
 *
 * See the `modelstat` daemon for the canonical client implementation.
 */
export const ProcessingMetadata = z.object({
  redacted_by: z.string().max(120).optional(),
  redaction_policy: z.string().max(80).optional(),
  redaction_policy_version: z.string().max(20).optional(),
  redactions_applied: z.number().int().min(0).optional(),
  compacted: z.boolean().optional(),
  summarized: z.boolean().optional(),
  bytes_saved: z.number().int().min(0).optional(),
  changes_applied: z.number().int().min(0).optional(),
  original_size_bytes: z.number().int().min(0).optional(),
  uploaded_size_bytes: z.number().int().min(0).optional(),
});
export type ProcessingMetadata = z.infer<typeof ProcessingMetadata>;

/**
 * One redaction policy definition the server publishes to clients.
 * Clients (agents, third-party SDKs) fetch /v1/policies and apply the
 * matching named policy locally; the client implementation lives in
 * @modelstat/agent. The server keeps an authoritative registry so a
 * sanitisation policy can be versioned, audited, and updated centrally.
 */
export const RedactionPolicy = z.object({
  name: z.string().max(60),
  version: z.string().max(20),
  description: z.string().max(400),
  redacts: z.array(z.string()).max(40),
  is_default: z.boolean().default(false),
  recommended_for: z.string().max(160).optional(),
});
export type RedactionPolicy = z.infer<typeof RedactionPolicy>;

/** One detected tool install on a device. */
export const DetectedInstallation = z.object({
  /** Open set — see `RawEvent.agent`. Discovery probes by artefact shape, so a
   * transcript store under a name nothing has enumerated is reported under that
   * name rather than not reported at all. */
  agent: z.string().max(120),
  /** Closed: WE classify the install method, so the set is ours to fix. */
  install_method: z.enum(INSTALL_METHODS),
  binary_path: z.string().nullable(),
  data_dir: z.string().nullable(),
  version: z.string().max(40).nullable(),
  detected_via: z.array(z.string()).max(6),
});
export type DetectedInstallation = z.infer<typeof DetectedInstallation>;

/** A source-account identity detected on the device. */
export const DetectedIdentity = z.object({
  /** Open set — see `RawEvent.provider`. The key-fingerprint probe reports the
   * provider name a user's own config states, which is not ours to enumerate. */
  provider: z.string().max(120),
  provider_account_id: z.string().max(200),
  provider_account_label: z.string().max(200).nullable(),
  /** Human-facing labels — what the user recognises the account by.
   *  Populated from the keychain blob / OAuth JWT where available. */
  account_email: z.string().max(200).nullable().optional(),
  account_org: z.string().max(200).nullable().optional(),
  display_name: z.string().max(200).nullable().optional(),
  owner_scope: z.enum(IDENTITY_OWNER_SCOPES).default("unassigned"),
  detection_source: z.string().max(80),
});
export type DetectedIdentity = z.infer<typeof DetectedIdentity>;

export const DiscoveryReport = z.object({
  device_id: z.string(),
  installations: z.array(DetectedInstallation).max(200),
  identities: z.array(DetectedIdentity).max(100),
  scanned_at: z.string().datetime({ offset: true }),
});
export type DiscoveryReport = z.infer<typeof DiscoveryReport>;

export const ClassificationConfidenceEnum = z.enum(CLASSIFICATION_CONFIDENCE);
