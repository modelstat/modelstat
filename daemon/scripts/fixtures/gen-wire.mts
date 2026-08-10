/**
 * Golden fixtures — §4.7 (wire) and the byte-clamp vectors (D12).
 *
 * Objects are constructed then run through the TS Zod schema's `.parse()` so the
 * emitted JSON is the canonical, defaults-applied form the server accepts. The
 * Rust golden test deserializes these (proving Rust accepts TS wire) and the TS
 * parity test re-parses both these and the Rust-emitted round-trips (proving TS
 * accepts Rust wire) — the two directions of D16.
 */
import { clampUtf8Bytes } from "../../../packages/daemon-core/src/http/clamp.js";
import {
  HeartbeatPayload,
  IngestBatch,
  RawEvent,
  Segment,
  ToolCallWire,
} from "../../../packages/core/src/schemas.js";
import { type Generator, writeGolden } from "./lib.mts";

const TS = "2026-06-01T10:00:00.000Z";
const TS_END = "2026-06-01T10:05:00.000Z";

const fullRawEvent = {
  source_event_id: "evt_16zw770jnvito",
  ts: TS,
  kind: "assistant_message",
  agent: "claude_code",
  provider: "anthropic",
  model: "claude-opus-4-7",
  session_id: "11111111-1111-1111-1111-111111111111",
  // The multi-agent primitives: WHICH agent-instance produced this turn, and —
  // on an inter-agent message — who it was addressed to. Both verbatim harness
  // ids; absent means the session's root actor.
  actor_id: "a628f67608a72832b",
  recipient_actor_id: "/root",
  turn_index: 3,
  parent_event_id: null,
  cwd: "/repo",
  git: {
    remote_url: "https://github.com/acme/app.git",
    remote_host: "github.com",
    remote_slug: "acme/app",
    branch: "main",
    slug_source: "git_remote",
  },
  tokens: { input: 10, output: 20, cache_creation: 0, cache_read: 5, reasoning: 0 },
  duration_ms: 1234,
  tool_calls: { Bash: 2, Read: 1 },
  files_touched: ["src/foo.ts", "src/bar.ts"],
  content_excerpt: "did some work on the ingest path",
  content_bytes: 32,
  // What the model was working out, as distinct from what it said. Same
  // redaction path as the prose.
  reasoning_excerpt: "checking the retry matrix before answering",
  reasoning_bytes: 41,
  references: { repos: [], pull_requests: [], issues: [] },
  source_file: "/data/session.jsonl",
  source_byte_offset: 4096,
};

// Minimal: required + nullable-as-null; defaulted + optional fields omitted so
// the fixture also pins default materialization (tool_calls {}, files_touched []).
const minimalRawEvent = {
  source_event_id: "evt_min",
  ts: TS,
  kind: "user_message",
  agent: "codex_cli",
  provider: "openai",
  model: null,
  session_id: "s-min",
  turn_index: null,
  parent_event_id: null,
  cwd: null,
  git: null,
  tokens: null,
  duration_ms: null,
  source_file: null,
  source_byte_offset: null,
};

const toolAction = {
  surface: "shell",
  executable: "kubectl",
  param_shape: "§ § § -n §",
  command_redacted: "kubectl rollout restart deploy/payments-api -n prod",
  extractor: "shell.v3",
};

const toolCall = {
  external_call_id: "toolu_abc",
  session_id: "11111111-1111-1111-1111-111111111111",
  source_event_id: "evt_16zw770jnvito",
  agent: "claude_code",
  server: "builtin",
  name: "Bash",
  turn_index: 3,
  call_index: 0,
  started_at: TS,
  ended_at: TS_END,
  status: "success",
  args_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  signature_hash: "none",
  args_bytes: 57,
  result_bytes: 128,
  model: "claude-opus-4-7",
  action: toolAction,
};

const segment = {
  segment_id: "seg_bf79y774ikaf",
  session_id: "11111111-1111-1111-1111-111111111111",
  agent: "claude_code",
  started_at: TS,
  ended_at: TS_END,
  abstract: "Implemented the ingest retry matrix and clamped wire strings.",
  tokens: { input: 100, output: 200, cache_creation: 0, cache_read: 0, reasoning: 0 },
  tags: [
    { root_key: "projects", name: "myrepo", confidence: 1.0 },
    { root_key: "work_types", name: "implementation", confidence: 0.7, reason: "branch heuristic" },
  ],
  redaction: { secrets_found: 1, emails_redacted: 0, paths_redacted_absolute: 2, pf_name: 1 },
  source_event_ids: ["evt_16zw770jnvito", "evt_irnlblnsf9gx"],
  behavior: { user_turns: 3, correction_count: 1, frustration: 0.25 },
  user_intent: "make the uploader never drop a batch",
  local_time: { utc_offset_minutes: -420, hour: 3, weekday: 1 },
};

const segmentWithEmbedding = {
  ...segment,
  segment_id: "seg_withemb",
  abstract_embedding: Array.from({ length: 384 }, () => 0),
};

const ingestBatch = {
  batch_id: "01HZ0000000000000000000000",
  device_id: "device-uuid-abc",
  daemon_version: "daemon-0.0.0",
  events: [fullRawEvent, minimalRawEvent],
  segments: [segment],
  tool_calls: [toolCall],
  // The actor REGISTRY the events' `actor_id`s join against — one entry per
  // agent-instance the harness stated it ran, every key present only when
  // stated. Two shapes on purpose: a Claude sub-agent (named by its own id,
  // described by its sidecar) and a codex sub-agent (named by its path inside
  // the harness's agent tree).
  session_actors: {
    "11111111-1111-1111-1111-111111111111": [
      {
        id: "a628f67608a72832b",
        label: "Explore",
        description: "Audit the alerting dashboards",
        spawn_tool_use_id: "toolu_abc",
        spawn_depth: 1,
        first_ts: "2026-06-01T10:00:00.000Z",
        last_ts: "2026-06-01T10:05:00.000Z",
      },
      {
        id: "/root/history_audit",
        path: "/root/history_audit",
        thread_id: "019f4b1d-2bb0-71c3-9173-345d5dd83f97",
        first_ts: "2026-06-01T10:01:00.000Z",
        last_ts: "2026-06-01T10:04:00.000Z",
      },
    ],
  },
  session_titles: { "11111111-1111-1111-1111-111111111111": "Ingest retry matrix" },
  summarizer_mode: "cloud",
  repo_anchors: [
    {
      slug: "acme/api",
      host: "github.com",
      cutoff: null,
      mined_at: "2026-06-01T10:00:00.000Z",
      head_sha: "0f4c9e7d2b8a1c6f3e5d7a9b0c2d4e6f8a1b3c5d",
      human_anchor_count: 1,
      ai_pr_count: 9,
      anchors: [
        {
          pr_number: 421,
          merge_sha: "9c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d",
          merged_at: "2025-11-20T14:30:00.000Z",
          files_changed: 12,
          lines_added: 340,
          lines_deleted: 85,
          span_ms: 259200000,
          commit_count: 7,
          active_minutes: 190,
          ai_assisted: false,
        },
      ],
    },
  ],
};

const heartbeat = {
  device_id: "device-uuid-abc",
  status: "scanning",
  message: "scanning 3/12 files",
  progress_done: 3,
  progress_total: 12,
  queue_size: 0,
  stats: { files_seen: 42 },
  last_event_at: TS,
  daemon_version: "daemon-0.0.0",
};

export const generator: Generator = {
  category: "wire (§4.7) + clamp (D12)",
  run: () => {
    writeGolden("wire/raw_event_full.json", RawEvent.parse(fullRawEvent));
    writeGolden("wire/raw_event_minimal.json", RawEvent.parse(minimalRawEvent));
    writeGolden("wire/tool_call.json", ToolCallWire.parse(toolCall));
    writeGolden("wire/segment.json", Segment.parse(segment));
    writeGolden("wire/segment_with_embedding.json", Segment.parse(segmentWithEmbedding));
    writeGolden("wire/ingest_batch.json", IngestBatch.parse(ingestBatch));
    writeGolden("wire/heartbeat.json", HeartbeatPayload.parse(heartbeat));

    // Byte-clamp vectors: input × cap → expected (from the TS clamp).
    const clampInputs: Array<{ name: string; s: string; max: number }> = [
      { name: "ascii_fits", s: "hello world", max: 320 },
      { name: "ascii_exact", s: "hello", max: 5 },
      { name: "zero_cap", s: "anything", max: 0 },
      { name: "cjk_cut_on_boundary", s: "日本語テキスト", max: 7 },
      { name: "emoji_atomic_drop", s: "a😀b", max: 4 },
      { name: "emoji_atomic_keep", s: "a😀b", max: 5 },
      { name: "accented", s: "café résumé", max: 6 },
      { name: "cjk_abstract_512", s: "字".repeat(400), max: 512 },
    ];
    writeGolden(
      "wire/clamp.json",
      clampInputs.map((c) => ({
        name: c.name,
        input: c.s,
        max_bytes: c.max,
        expected: clampUtf8Bytes(c.s, c.max),
      })),
    );
  },
};
