# Golden fixtures — the wire-contract spine

These files are the frozen contract the daemon, the SDKs, the Chrome extension
and the server all derive ids and payloads against (plan §4). Each one has an
OWNER — the implementation that regenerates it — and a set of tests that assert
against it. Do not hand-edit any of them.

| Owner | Regenerate with | Files |
|---|---|---|
| Live TypeScript (`packages/core`, `packages/daemon-core`) | `npx tsx daemon/scripts/fixtures/gen-all.mts` | `ids.json`, `param_shape.json`, `enums.json`, `redaction.json`, `wire/*.json`, `file_formats/*`, `cli/*`, `summarizer/*` |
| Rust (`modelstat-wire`) | `cargo run -p modelstat-wire --example emit_fixtures` | `wire/rust-emitted/*.json` |
| Rust (`modelstat-parsers`) | `REGEN_GOLDENS=1 cargo test -p modelstat-parsers --test golden_parsers regen` | `parsers/*.json` |
| **Nobody — frozen** | *(never regenerated)* | `device.json`, `shell_executable.json`, `tool_name.json` |

The frozen three are the vectors whose TypeScript side lived in the retired
daemon and is now deleted. Freezing is deliberate: a fixture the implementation
can rewrite pins nothing, so these stay exactly as the TS produced them and the
Rust must keep matching. `golden_ids.rs` asserts `device.json`;
`modelstat-parsers/tests/golden_tooling.rs` asserts the other two.

CI runs every regenerator and gates on `git diff --exit-code`, so any change that
would move a fixture (i.e. break id/redaction/wire parity) fails loudly.

## Categories (feature §4)

| File(s) | §4 | Provenance |
|---|---|---|
| `ids.json` | 4.1 | Run TS `sourceEventId` / `segmentId` / `fallbackCallId` (`packages/core/src/ids.ts`). |
| `device.json` | 4.1 | FROZEN. Was TS `deviceUuidFromMachineKey`; machine-key hash uses the frozen §4 salt. |
| `param_shape.json` | 4.2 | Run TS `paramShape` (`packages/core/src/ids.ts`). |
| `shell_executable.json`, `tool_name.json` | 4.2 | FROZEN. Were TS `extractExecutable` / `normalizeToolName` + `splitObservedToolName`. |
| `redaction.json` | 4.3 | Run TS wire floor `redact()`; every SECRET_FLOOR pattern + entropy branches + non-redaction guarantees. |
| `parsers/*.json` | 4.4 | Run the actual RUST parsers (claude-code / codex / pi / cursor) over fixed transcripts materialized under `/tmp/modelstat-fixtures` (stable ids). Rust-owned since SPEC 0005. |
| `file_formats/*.json` | 4.5 | Recorded snapshots matching the TS on-disk interfaces (identity / runtime-state / file-queue-store); includes a legacy `state.json` the Rust must read tolerantly. |
| `cli/*.json` | 4.6 | Recorded snapshots of the CLI `--json` shapes frozen in feature §5. |
| `wire/*.json` | 4.7 | Objects run through the TS Zod schemas' `.parse()` (defaults applied); plus byte-clamp vectors from `clampUtf8Bytes`. |
| `wire/rust-emitted/*.json` | 4.7 | Rust's re-serialization of the `wire/*.json` above (written by `cargo run -p modelstat-wire --example emit_fixtures`). The TS parity test parses these to prove **TS accepts Rust wire** (D16, the second direction). |
| `summarizer/*.json` | 4.8 | Recorded protocol-v1 request/response snapshots per feature §10.4 (the protocol is new — no TS predecessor). |

## How parity is proven both ways (D16)

- **Rust accepts TS wire** — `tests/golden_wire.rs` deserializes every `wire/*.json`
  into the Rust structs, checks key fields + strictness, and round-trips.
- **TS accepts Rust wire** — `packages/core/src/wire-parity.test.ts` re-parses both
  `wire/*.json` and `wire/rust-emitted/*.json` through the Zod schemas.

Categories 5, 6 and 8 are recorded snapshots authored to the on-disk / CLI /
protocol shapes that define them, rather than the output of running anything.
