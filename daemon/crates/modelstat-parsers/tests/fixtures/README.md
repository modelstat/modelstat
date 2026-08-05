# Parser fixtures

These are **real transcripts, produced by the agents themselves** — not
hand-authored JSON. That is deliberate: the previous Cursor fixture was
invented, and inventing it is exactly why nobody noticed that the table the
parser read (`ai_code_hashes`) no longer exists in Cursor. A fictional fixture
lets a parser pass against a world that isn't there.

| fixture | provenance |
|---|---|
| `tree/codex/rollout-*.jsonl` | A real `codex exec` run (codex-cli 0.147.0-alpha) against a throwaway `uploader.js` in a neutral temp directory. |
| `tree/cursor/state.vscdb` | Two real Cursor conversations from a live `globalStorage/state.vscdb`. |
| `tree/claude-desktop/*.jsonl` | A real Claude Desktop local-agent-mode session — the same Claude Code format, from the desktop app's own data dir. |
| `tree/claude*`, `tree/pi` | Pre-existing fixtures. |

## What was removed, and why

Nothing was added or altered — only whole records or whole fields were dropped,
so every value that remains is verbatim as the agent wrote it:

- **codex**: the `session_meta.base_instructions` blob (~18 KB of the vendor's
  own system prompt) and two lines carrying local home paths (the skills index
  and `world_state`). The parser reads none of them.
- **cursor**: each bubble keeps only the fields that make it a message
  (`_v`, `bubbleId`, `type`, `text`, `createdAt`, `tokenCount`, `isAgentic`).
  The rest of a real record is attached working context — code blocks, diffs,
  lint results, file chunks — which is megabytes the parser never reads and
  where local paths live.

- **claude-desktop**: the machine's user name only (`/Users/<name>` →
  `/Users/dev`, and the same inside the encoded project-dir name). Unlike the
  other two, a path here is a load-bearing field — `cwd` drives repo detection —
  so the record cannot simply be dropped. The substitution is mechanical and
  preserves every record's shape exactly; no structure is invented.

All fixtures are checked to contain no e-mail addresses or credential-shaped
strings, and no home path other than the placeholder above.

## Regenerating the goldens

Fixture inputs live here; the expected outputs live in
`crates/modelstat-wire/tests/golden/parsers/`. After changing a parser:

```bash
REGEN_GOLDENS=1 cargo test -p modelstat-parsers --test golden_parsers regen
```

Then **read the diff** — it is the parser's behaviour change, stated.
