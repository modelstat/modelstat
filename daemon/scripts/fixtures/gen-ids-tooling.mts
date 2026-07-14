/**
 * Golden fixtures — §4.1 (ids) and §4.2 (paramShape / shell.v3 executable /
 * normalizeToolName), generated from the TS implementations.
 */
import { createHash } from "node:crypto";
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
import { paramShape, segmentId, sourceEventId } from "../../../packages/core/src/ids.js";
import { deviceUuidFromMachineKey } from "../../../apps/daemon/src/machine-key.js";
import { extractExecutable } from "../../../packages/parsers/src/tool-action/executable.js";
import {
  fallbackCallId,
  normalizeToolName,
  splitObservedToolName,
} from "../../../packages/parsers/src/tool-hash/index.js";
import { type Generator, writeGolden } from "./lib.mts";

/** The frozen machine-key salt (feature §4/§18). Not exported from
 * machine-key.ts, so replicated here verbatim; the Rust MACHINE_KEY_SALT const
 * carries the identical literal. */
const MACHINE_KEY_SALT = "modelstat.device.machine-key.v1";

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

  // --- device: machine-key hash + deterministic UUIDv5 (feature §4) ---
  const machine_key_hash = ["abc", "IOPlatform-UUID-1234", ""].map((raw) => ({
    raw,
    expected: createHash("sha256").update(`${MACHINE_KEY_SALT}:${raw}`).digest("hex"),
  }));
  // 64-char synthetic keys. The third is a clearly-fake, gitleaks-allowlisted
  // value (a sha256-shaped literal trips the generic-api-key entropy rule).
  const deviceKeys = ["0".repeat(64), "a".repeat(64), `examplefake${"0123456789".repeat(5)}abc`];
  const device_uuid = deviceKeys.map((key) => ({ key, expected: deviceUuidFromMachineKey(key) }));
  // Salted path (intendedDeviceUuid appends `:<salt>` before deriving).
  const device_uuid_salted = [
    { machine_key: "a".repeat(64), salt: "ci-2" },
    { machine_key: "a".repeat(64), salt: "tenant-b" },
  ].map((c) => ({ ...c, expected: deviceUuidFromMachineKey(`${c.machine_key}:${c.salt}`) }));

  writeGolden("device.json", { machine_key_hash, device_uuid, device_uuid_salted });
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

  // --- shell.v3 executable extraction ---
  const shellInputs = [
    "kubectl rollout restart deploy/payments-api -n prod",
    "cd x && git push",
    "./deploy.sh --now",
    "sudo systemctl restart nginx",
    "FOO=bar realcmd --flag",
    "WT=$(ssh host uptime) && echo $WT",
    "ls -la",
    "echo hello world",
    "cd ~",
    "# just a comment",
    'CK="sk_live_examplefake0123456789" node index.js', // synthetic (gitleaks-allowlisted markers)
    "for i in 1 2 3; do curl https://x; done",
    "pnpm -C packages/core test",
  ];
  writeGolden(
    "shell_executable.json",
    shellInputs.map((command) => ({ command, expected: extractExecutable(command) })),
  );

  // --- normalizeToolName + splitObservedToolName ---
  const normalizeInputs = [
    "  Bash  ",
    "café", // NFD → NFC é
    "subscribe_a1b2c3d4e5f6",
    "job-550e8400-e29b-41d4-a716-446655440000",
    "create_pr",
    "x".repeat(300),
  ];
  const splitInputs = [
    "mcp__github__create_pr",
    "mcp__brave-search__web_search",
    "mcp__my_server__tool",
    "Bash",
    "WebSearch",
  ];
  writeGolden("tool_name.json", {
    normalize: normalizeInputs.map((input) => ({ input, expected: normalizeToolName(input) })),
    split: splitInputs.map((input) => ({ input, ...splitObservedToolName(input) })),
  });
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
