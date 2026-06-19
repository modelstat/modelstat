# Contributing to modelstat

Thanks for taking the time. This is the daemon-code repo — the Node
daemon, macOS tray, MCP server, and their shared packages. The hosted
service isn't in scope here.

## Before you start

- Read the **[README](README.md)** to understand what's in here.
- Read **[SECURITY.md](SECURITY.md)** if you found a vulnerability —
  do not open a public issue.
- Read the **[LICENSE](LICENSE)** — using the code for yourself is
  fine; running a hosted competing service is not.

## Running locally

You need:

- **Node** 20.18+
- **pnpm** 10.7+
- **Swift** 5.9+ (only for the macOS tray)
- A modelstat account to pair against ([free tier](https://modelstat.ai/pricing))

```bash
git clone https://github.com/modelstat/modelstat.git
cd modelstat
pnpm install
pnpm build
pnpm typecheck
```

Per-component:

```bash
pnpm --filter modelstat build           # Node daemon
pnpm --filter @modelstat/mcp build             # MCP server
pnpm tray-mac:build                            # macOS tray
```

Point the daemon at a local / self-hosted API:

```bash
DAEMON_API_URL=http://localhost:3010 node apps/daemon/dist/cli.cjs connect
```

## What kinds of contributions we'd love

### 1. New tool integrations

Adding support for a new AI coding tool (say, a newly-launched CLI) is
the highest-impact thing you can contribute.

Where: [`packages/parsers/src/`](packages/parsers/src/)

Shape: each parser is a module that reads the tool's local artifacts
(JSONL, SQLite, log file) and emits canonical `RawEvent[]` per the
schema in `@modelstat/core`. Existing parsers for Claude Code, Codex,
and Cursor are good reference material.

Workflow:

1. Open an issue describing the tool + where it stores its data
   (`~/.xxx/...`, a SQLite DB, etc.) so we can align on the canonical
   event shape.
2. Add detection logic in [`packages/parsers/src/discovery/`](packages/parsers/src/discovery/).
3. Add the parser module + tests.
4. Add the tool slug to the enum in `packages/core/src/enums.ts`.
5. Open a PR with a 30-line sample of the tool's raw log format.

### 2. Redaction patterns

Missing a secret shape the `strict-pii-v2` policy should catch?

Where: [`packages/daemon-sdk/src/redact.ts`](packages/daemon-sdk/src/redact.ts)

Add a `Pattern` entry to the right category (`SECRETS`,
`MODELSTAT_SECRETS`, or `PII`) with a regex, a name, and a
replacement label. Include an adversarial test in the PR
description (we don't have a formal test runner yet; we're
adding Vitest).

### 3. Docs + README polish

PRs that improve the quickstart, fix typos, or add screenshots to
the README are gratefully accepted. The README is both a human
document and an AI-crawled document (ChatGPT / Claude / Perplexity
cite it) — clarity helps both audiences.

### 4. Bug fixes + refactors

Please open an issue first for anything that changes behavior
beyond a localized fix. Refactor-for-the-sake-of-refactor PRs will
usually be declined — the project has a deliberate simplicity bias.

## What we'll probably decline

- Changing the license or the competing-use carve-out.
- Adding telemetry that isn't strictly needed to render the
  dashboard (we over-redact, by design).
- Importing any non-trivial runtime dependency.
- Rewriting a component in a different framework.
- PRs against `main` without a linked issue for anything larger
  than a one-liner.

## Pull request checklist

- [ ] Linked issue or clear motivation in the PR description.
- [ ] `pnpm typecheck` passes.
- [ ] `pnpm lint` passes (Biome).
- [ ] No new runtime dependencies without discussion.
- [ ] No server-only imports sneaking in.
- [ ] Commit messages follow the project style — lowercase prefix
      (`feat:`, `fix:`, `chore:`, `refactor:`), one-line subject
      under 70 chars, paragraph body explaining the "why".

## Code of conduct

Be kind, be specific, assume good faith. Don't share other users'
data in issues or PRs — the daemon handles sensitive file paths and
tool logs; please redact before attaching samples.

## Questions

- General: [hello@modelstat.ai](mailto:hello@modelstat.ai)
- Security: [security@modelstat.ai](mailto:security@modelstat.ai)
- GitHub Issues: [github.com/modelstat/modelstat/issues](https://github.com/modelstat/modelstat/issues)
