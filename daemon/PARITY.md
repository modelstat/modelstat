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

## What is NOT yet ported (by milestone, not omission)

M0 is contracts only. `shell.v3` executable extraction and `normalizeToolName`
have committed golden fixtures (`shell_executable.json`, `tool_name.json`) but
their Rust impls land in **M2** (`modelstat-parsers`); the file-format / CLI /
summarizer-protocol snapshots are consumed in **M1/M4/M5/M6**. See plan §5.
