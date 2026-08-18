/**
 * Golden fixtures — §4.1 (ids), §4.2 (paramShape) and the enum arrays,
 * generated from the TypeScript implementations that are still LIVE
 * (`packages/core`, which the Chrome extension and the MCP server ship).
 *
 * Three fixture families in this directory are deliberately NOT generated here:
 *
 *   - `device.json` — the §4 machine-key hash + deterministic device UUID. Their
 *     TS implementation lived in the retired TypeScript daemon and is deleted,
 *     so the committed vectors are now a FROZEN contract: `modelstat-wire`'s
 *     `golden_ids.rs` asserts the Rust derivations against them and nothing
 *     regenerates them. Frozen is the point — a value that can be rewritten by
 *     the implementation it is meant to pin is not a gate.
 *   - `shell_executable.json`, `tool_name.json` — `extractExecutable` /
 *     `normalizeToolName` / `splitObservedToolName` are Rust-only now
 *     (`modelstat-parsers`). Same deal: frozen vectors, asserted by
 *     `modelstat-parsers`' `golden_tooling.rs`.
 *   - `parsers/*.json` — Rust-generated since SPEC 0005 (see gen-all.mts).
 */
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
} from "../../../packages/core/src/enums.js";
import {
  fallbackCallId,
  paramShape,
  segmentId,
  sourceEventId,
} from "../../../packages/core/src/ids.js";
import { type Generator, writeGolden } from "./lib.mts";

function idsFixtures(): void {
  // --- source_event_id (all three shapes; device partitions the key space) ---
  const sourceEventCases = [
    { device_id: "dev_1", source: { type: "file", file: "/x/a.jsonl", byte_offset: 42 } },
    { device_id: "dev_1", source: { type: "file", file: "/path/with spaces/日本語.jsonl", byte_offset: 0 } },
    { device_id: "dev_1", source: { type: "file", file: "/x/a.jsonl", byte_offset: 9007199254740991 } },
    { device_id: "dev_1", source: { type: "line_uuid", line_uuid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee" } },
    { device_id: "dev_2", source: { type: "line_uuid", line_uuid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee" } },
    { device_id: "dev_1", source: { type: "web", host: "chatgpt.com", conversation_id: "c1", message_id: "m1" } },
  ] as const;
  const source_event_id = sourceEventCases.map((c) => {
    const s = c.source;
    let expected: string;
    if (s.type === "file") expected = sourceEventId(c.device_id, { file: s.file, byteOffset: s.byte_offset });
    else if (s.type === "line_uuid") expected = sourceEventId(c.device_id, { lineUuid: s.line_uuid });
    else expected = sourceEventId(c.device_id, { host: s.host, conversationId: s.conversation_id, messageId: s.message_id });
    return { ...c, expected };
  });

  // Legacy 3-arg form must equal the object form (wire contract).
  const legacy_equivalence = {
    three_arg: sourceEventId("dev_1", "/x/a.jsonl", 42),
    object_form: sourceEventId("dev_1", { file: "/x/a.jsonl", byteOffset: 42 }),
  };

  // --- segment_id (source_event_ids are SORTED before hashing) ---
  const segmentCases = [
    { session_id: "s1", started_at_ms: 1000, ended_at_ms: 2000, source_event_ids: ["evt_b", "evt_a"] },
    { session_id: "s1", started_at_ms: 1000, ended_at_ms: 2000, source_event_ids: ["evt_a", "evt_b"] },
    { session_id: "11111111-1111-1111-1111-111111111111", started_at_ms: 0, ended_at_ms: 0, source_event_ids: [] },
    { session_id: "s2", started_at_ms: 1717236000000, ended_at_ms: 1717239600000, source_event_ids: ["evt_16zw770jnvito", "evt_irnlblnsf9gx"] },
  ] as const;
  const segment_id = segmentCases.map((c) => ({
    ...c,
    expected: segmentId(c.session_id, c.started_at_ms, c.ended_at_ms, c.source_event_ids),
  }));

  // --- tc_ fallback external_call_id ---
  const tcCases = [
    { source_event_id: "evt_abc", call_index: 0 },
    { source_event_id: "evt_abc", call_index: 1 },
    { source_event_id: "evt_16zw770jnvito", call_index: 7 },
  ] as const;
  const tc_fallback_id = tcCases.map((c) => ({
    ...c,
    expected: fallbackCallId(c.source_event_id, c.call_index),
  }));

  writeGolden("ids.json", { source_event_id, legacy_equivalence, segment_id, tc_fallback_id });
}

function toolingFixtures(): void {
  // --- paramShape (the ids.test.ts table + UTF-16/whitespace edge cases) ---
  const paramInputs = [
    "rollout restart deploy/payments-api -n prod",
    'commit -m "fix bug"',
    "-la /etc/passwd",
    "install react",
    "--namespace=prod get pods",
    "run --watch -j4 test/unit",
    "status",
    "",
    "   ",
    "deploy\tprod\nregion=us", // tab + newline are separators; `\v` is NOT
    "--label=café --emoji=😀 plain",
  ];
  writeGolden("param_shape.json", paramInputs.map((input) => ({ input, expected: paramShape(input) })));
}

function enumsFixture(): void {
  // The canonical enum arrays (order + membership are the contract). The Rust
  // `modelstat_wire::enums` arrays must equal these exactly.
  writeGolden("enums.json", {
    agents: AGENTS,
    providers: PROVIDERS,
    event_kinds: EVENT_KINDS,
    tool_call_statuses: TOOL_CALL_STATUSES,
    os_families: OS_FAMILIES,
    daemon_phases: DAEMON_PHASES,
    install_methods: INSTALL_METHODS,
    identity_owner_scopes: IDENTITY_OWNER_SCOPES,
    classification_confidence: CLASSIFICATION_CONFIDENCE,
  });
}

export const generator: Generator = {
  category: "ids + tooling (§4.1, §4.2)",
  run: () => {
    idsFixtures();
    toolingFixtures();
    enumsFixture();
  },
};
