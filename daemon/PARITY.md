# Parity decisions — Rust rewrite vs the TypeScript daemon

The rewrite must stay byte-for-byte compatible with the shipping TS daemon on
ids, device identity, wire schemas, and the redaction floor (session task rules;
feature §21, plan §4). This file records the non-obvious decisions a reviewer
would otherwise have to reverse-engineer. Add a row here (and, for intentional
divergences, a line in feature §23) rather than leaving a silent choice.

## Faithful ports (behavior identical, implementation adapted)

| Area | Decision |
|---|---|
| **djb2-64** (`evt_`/`seg_`/`tc_` ids) | Hash over **UTF-16 code units** (`str::encode_utf16`), matching JS `charCodeAt` — not bytes or code points. `h.wrapping_mul(33) ^ (unit as u64)` equals JS's `(h*33)^c` masked to 64 bits (AND distributes over XOR). base36 matches `BigInt.toString(36)` (lowercase, `"0"` for zero). |
| **Device UUIDv5** | `SHA-1(namespace_bytes ++ utf8(machineKeyHex))`, version/variant forced. The salt path (`MODELSTAT_DEVICE_SALT`) hashes `"<key>:<salt>"`, exactly as TS `intendedDeviceUuid`. |
| **Machine-key hash** | `SHA-256("modelstat.device.machine-key.v1:" + raw)`. The salt const is frozen (feature §18); it is not exported from the TS, so it is replicated verbatim and the fixture generator computes the golden with the same literal. |
| **paramShape** | Splits on exactly `[ \t\n\r\f]+` (NOT the full Unicode `\s`, and NOT vertical tab), matching the JS regex character class. |
| **Enums** | Carried as validated `String`s, not Rust enums (the sets are large/churny and the daemon must round-trip a value a newer server knows). `enums.json` pins order+membership against the TS arrays. |
| **Byte clamp** | `clamp_utf8_bytes` iterates code points (`chars()`), matching JS `for…of`; astral chars kept/dropped atomically. Drives off declared caps, not a hand-listed field set (the field-by-field version shipped the CJK-400 bug). |
| **Redaction floor** | `regex` crate with `(?-u:\b)` for ASCII word boundaries and ASCII classes spelled out (`\d`→`0-9`, `\w`→`A-Za-z0-9_`) to match JS semantics. The wire floor replaces the **whole match** with `[REDACTED:<name>]` (the catalogue's replacement templates are for the SDK-side redactor, ported in M3). |
| **`aws_secret_key`** | JS uses lookaround (`(?<!…){40}(?!…)`) the `regex` crate can't express. Implemented as an exact-length run scan — provably equivalent (a lookaround-bounded run matches iff a maximal class run is exactly 40 chars). |
| **Entropy pass** | Shannon entropy sums floats in **first-occurrence order** (JS `Map` insertion order) so the running total is bit-identical and a threshold decision can't flip on float non-associativity. Candidates are pure ASCII, so `chars().count()` == JS `s.length`. |

## Intentional additions / divergences (also noted in feature §23 where applicable)

| Area | Decision | Why it's safe |
|---|---|---|
| **Agent enum count** | Spec §4 prose says "34-enum"; the TS `AGENTS` array has **33**. Rust matches the **code** (33), per the spec's own "code wins" rule (§23). | Fixture `enums.json` is generated from the TS array, so the two can't disagree. |
| **Windows redaction paths** | `C:\Users\…`, `%USERPROFILE%`, and UNC `\\host\share` are redacted (feature §17.4 mandates them; the TS had none). No TS golden exists, so they're covered by Rust-only unit tests in `paths.rs`. | Additive (strictly more privacy); lands under PROCESSING_VERSION 16; doesn't affect the TS-derived redaction goldens (which contain no Windows paths). |
| **`references` / `session_metadata`** | Modeled as opaque `serde_json::Value` passthrough, not typed structs. | Lossless round-trip (more faithful than a hand re-serialization); the full typed port lands with the M4 enrichment work. |
| **`state.json.selfHostedModel`** | Not a field on the Rust `state` struct — read-tolerated, ignored, dropped on next write (feature §19). | The legacy-state golden (`file_formats/state_legacy.json`) carries it so the M1 reader is tested against it. |

## M1 — identity, config, device API (structural + behavioral decisions)

| Area | Decision | Why it's safe / faithful |
|---|---|---|
| **Foundation crate placement** | paths, machine-key probes, the identity + state stores, `Config`, and the device-API client all live in **`modelstat-ingest`**, not `modelstat-daemon`. | Forced by the frozen dep graph: `recoverIdentity` is spec-assigned to `modelstat-ingest` (plan §3) and needs config + identity; `modelstat-daemon` *depends on* `modelstat-ingest`, so the foundation can't live in daemon without a cycle; `modelstat-wire` is pure-contract (no I/O). `ingest` is the lowest I/O crate below both `daemon` and `cli`. The M0 `device.rs` comment guessing "M1 (`modelstat-daemon`)" is superseded by the graph. |
| **`Config` = explicit struct** | The TS module-level singleton `state` (cached identity + module-level recover backoff) is modeled as a `Config` you construct and pass, identity behind a `Mutex`; the recover backoff lives on the `DeviceApi` instance. | Behaviour is identical — write-through setters, same api-url resolution order, same `[0,2,5,15,30,60]s` recover schedule. The struct makes the five e2e scenarios drivable in one process and keeps env-dependent state out of process globals. |
| **machine-key fallback honors `MODELSTAT_HOME`** | The fallback key file is `home_path("machine-key")`, not a hard-coded `~/.modelstat/machine-key`. | The one intentional §23 fix. Hardware-id devices (the vast majority) are unaffected; only a fallback-key device that *relocated* may re-derive (documented, acceptable). Unit-tested via `fallback_key_file_honors_modelstat_home`. |
| **`os_family` gains `windows`** | `build_fingerprint` maps Windows → `"windows"`; the TS returned `"other"`. | Feature §4 mandates it and `OS_FAMILIES` (in `modelstat-wire`) already carries `"windows"`. Additive; the server enum accepts it. |
| **`os_version`** | `uname -r` on Unix (byte-identical to Node `os.release()` on macOS/Linux); `cmd /c ver` parse on Windows. | Not a golden-tested value (inherently machine-specific); the server just stores/displays it. Windows service specifics land in M5. |
| **self-hosted URL override** | `MODELSTAT_SUMMARIZER_URL` overrides the stored URL; the `MODELSTAT_LLM_*` family is gone. | Feature §19/§23 (BYO endpoints dropped, one env var replaces the pair). |
| **`--fresh` convergence** | Proven by the `e2e_m1` integration test and `scripts/e2e-m1.sh` via *back up + wipe identity + re-register* — the exact machine-stable path (feature §21.9). The `connect --fresh` **command** lands in M6. | The convergence mechanism (derive same uuid → dedupe on `machine_id` → same `device_id`) is M1 and fully exercised; only the `connect` wrapper is deferred. Staying inside the milestone boundary. |
| **Fake device-API server** | The e2e is proven against an in-process axum fake server (dev-dependency only — never linked into the collector) that reproduces the real server's `{data:…}` envelope + `machine_id` dedupe (verified against `core/rust/crates/api/src/devices.rs`). | Docker isn't available here to stand up the real Postgres/ClickHouse/MinIO + API stack; plan §6 explicitly sanctions a "fake … server harness". `scripts/e2e-m1.sh` runs the same flows against a real `$DAEMON_API_URL` when one is provided. |
| **`token` / prod-guard copy** | `token` unpaired hint and the prod-guard "run it interactively" remedy say `modelstat`, not the TS `npx modelstat@latest`. | The npm path is dropped (§22); the binary is `modelstat`. Not a golden-tested string (stderr help text); the AC checks the **exit code** (1 / 2). |
| **`cli/status.json` api-vs-dashboard note (not M1)** | The M0 `status.json` golden pairs `api: api.modelstat.ai` with `dashboard: modelstat.ai/dashboard`, which is inconsistent with the TS `dashboard = <apiUrl>/dashboard` derivation. | Flagged here for **M5** (the `status` command) to resolve deliberately. M1 keeps `DEFAULT_API_URL = https://modelstat.ai` (TS parity); the `paths`/`token` `api` field is just `state.apiUrl`. |

## M2 — parsers, discovery, tool-action (structural + behavioral decisions)

| Area | Decision | Why it's safe / faithful |
|---|---|---|
| **`serde_json` `preserve_order`** (workspace) | Enabled so `Value` objects serialize in insertion order, not sorted. | `args_hash` = sha256 over the tool input exactly as `JSON.stringify` emits it (source-key order); a `Value` re-serialized with sorted keys would hash differently. Only affects `serde_json::Value`; struct-field `BTreeMap`s are unchanged. Side effect: the Rust-emitted `references` blob now matches the TS field order `{repos, pull_requests, issues}` — it previously serialized sorted (`{issues,…}`), a latent mismatch this fixes. |
| **Byte offsets** | Each line advances `utf8_len(line_without_newline) + 1`; `source_byte_offset` captured before the advance (`line_reader.rs`). | Byte-exact vs the TS `Buffer.byteLength(line)+1` on LF files (every fixture); on CRLF it drifts identically to the TS, so `source_event_id` matches either way. |
| **Resume-copy dedupe** | `AncestorCache` does the same-dir probe then a one-level sibling-dir walk under the projects root, memoised. A line whose `sessionId` ≠ the filename uuid is dropped when the ancestor `<sid>.jsonl` exists; else emitted keyed by `uuid::<lineUuid>`. | Byte-for-byte port of `dedupeIdFor`/`ancestorFileExists`; the `claude_synthetic` (all lines dropped) + `claude_resume_copy` goldens exercise both paths. |
| **codex `event_msg` timestamp** | TS falls back to `new Date().toISOString()` when a line has no `timestamp`; the Rust port falls back to the last-seen line timestamp instead (else skips the event). | Determinism > wall-clock (a replayed scan must re-derive identical ids/ts); codex always writes timestamps, so this never differs in practice. Noted as an intentional §23-class divergence. |
| **Cursor snapshot** | Opens a byte-snapshot COPY of the `.vscdb` read-only via `rusqlite` `bundled` (read → temp → open), never the live file (plan D6). Parser stays dormant behind `MODELSTAT_ENABLE_CURSOR_PARSER` (§7.1) — only its output shape is frozen. | Static-linked SQLite; snapshot avoids locking a DB Cursor holds open. `sql.js` (WASM) is dropped — a Node runtime dep the collector can't have. |
| **`references` mining** | `detectReferences`/`detectEventReferences` are ported into `modelstat-parsers::references` (the parsers call them at parse time), producing an opaque `serde_json::Value` on `RawEvent.references`. The full typed `SessionMetadata` + git-outcome/`--numstat` enrichment stays M4. | The parser must not silently never-detect references (that would be a stub); the miner is pure + tested. Modeling the output as a `Value` matches the wire schema's passthrough decision (row above). |
| **Regex ASCII classes** | `\w`/`\d`/`\b`/`\s` spelled out as ASCII (`[0-9A-Za-z_]`, `(?-u:\b)`, …) since Rust's `regex` is Unicode-by-default while JS's are ASCII; a literal `[` inside a character class is escaped (`\[`) because Rust reads a bare `[` as a nested-class opener. | Matches JS regex semantics exactly (the `regex` crate has no direct `\b`-ASCII shorthand). |
| **`normalize_tool_name` cap** | Truncates to 120 **UTF-16 code units** (JS `String.slice`), not bytes/code points, via `slice_utf16`. | Faithful to JS; BMP-only tool names (the universe) slice identically to a char count. |
| **Fixture reconstruction** | The parser golden test rebuilds the exact `/tmp/modelstat-fixtures` tree (committed under `tests/fixtures/tree/`) and parses at those canonical paths; `#[cfg(unix)]`. | `source_event_id` embeds the `/tmp/modelstat-fixtures/...` path, so parity requires the generator's documented fixed base path; Windows offset/path parity is out of scope for the golden suite (the generator runs on Linux/macOS). |

## What is NOT yet ported (by milestone, not omission)

M0 is contracts only. `shell.v3` executable extraction and `normalizeToolName`
have committed golden fixtures (`shell_executable.json`, `tool_name.json`) but
their Rust impls land in **M2** (`modelstat-parsers`); the file-format / CLI /
summarizer-protocol snapshots are consumed in **M1/M4/M5/M6**. See plan §5.

**M1 (`modelstat-ingest` + `modelstat-cli`) ports:** paths/home, machine-key
probes + fingerprint, the identity + state stores, `Config`, the device-API
client (register / devices-me / recover / heartbeat + the retry matrix), and the
`self-register` / `await-claim` / `token` / `paths` commands. Still deferred:
the reconcile/backfill endpoints that *use* the device matrix (M4), the
`IngestClient` upload path (M4), and the rest of the CLI surface (M4–M6).
