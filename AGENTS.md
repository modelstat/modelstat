# AGENTS.md — working on the modelstat companion

Guidance for developers and coding agents in this repo. Keep it current:
when you change how something here works, update this file in the same PR.

## What this repo is

The modelstat **companion** — everything that runs on a user's machine and
feeds the server: the Node CLI/agent (`packages/*`, published to npm +
Homebrew), the Chrome extension (`apps/extension`), and the macOS tray app
(`apps/tray-mac`). The server (ingest/pipeline/dashboard, modelstat.ai)
is a separate private service (closed-source) and is out of scope for
this repo.

## Build & test

```sh
pnpm install
pnpm test          # turbo; per-package: node --import tsx --test src/**/*.test.ts
pnpm typecheck
pnpm build
```

Tests run through the **tsx loader** (`node --import tsx --test`) — do not
switch to `--experimental-strip-types`; it's broken on Node 20 and doesn't
resolve `.js` → `.ts` imports.

Things to know:

- `prices/*.yaml` are deliberate placeholders, not real prices. Don't
  "fix" them and don't write tests that assert specific dollar amounts.
- Parsers (`packages/parsers`) emit raw events only and keep transcript
  data verbatim — e.g. the `<synthetic>` pseudo-model Claude Code records
  for local error/notice messages is passed through as-is (the server
  decides what to hide; the companion never drops data). The one exception:
  `<synthetic>` must not update the parser's `lastModel` attribution state.
- The summariser is npm-only, single-path (no Ollama, no fallbacks),
  staged via `installNativeRuntime`/`_setup-runtime`.
- Redaction has **one floor**. The secret-pattern catalogue lives in
  `@modelstat/core/redact-floor` (dependency-free) and is the single source of
  truth for both the wire redactor (`@modelstat/core/redact`) and the published
  SDK redactor (`@modelstat/agent-sdk`) — add a newly-leaked credential format
  there, once. The server can *augment* it at runtime via a signed, additive
  `policies` config: the floor always applies and a signed bundle can only ever
  add patterns, never remove or weaken them.
- `@modelstat/remote-config` is the shared signed-config loader (fetch → verify
  Ed25519 over raw bytes → disk-cache under `~/.modelstat/config/` → fall back
  memory→disk→bundled). The long-lived daemon refreshes the `policies` kind on a
  timer; new server-delivered config kinds ride this loader instead of forcing a
  release.

## Releasing (npm + Homebrew)

Releases are **manual** — merging to main publishes nothing. The one-click
`Release` workflow (`.github/workflows/release.yml`) automates the entire
chain: bump `package.json` → build → npm publish → commit the bump to main
→ tag `<pkg>-v<version>` → GitHub Release → (agent only) Homebrew tap bump.

```sh
gh workflow run release.yml -f package=agent -f release_type=patch
# package: agent | mcp;  release_type: patch | minor | major | none
# or -f version=X.Y.Z to pin an exact version
```

(or GitHub → Actions → Release → Run workflow.)

### Observing a release

```sh
gh run list --workflow=release.yml --limit 3
gh run watch <run-id> --exit-status
```

Verify the artifact landed: `npm view modelstat version` (agent) and the
GitHub Release/tag exist.

### When a release fails

```sh
gh run view <run-id> --log-failed
```

- npm `E403` → the `NPM_TOKEN` repo secret lacks publish rights (it must
  be an npm **Automation** token): `gh secret set NPM_TOKEN`.
- The workflow publishes to npm BEFORE pushing anything to main, and skips
  publish if the version already exists — so a failed run leaves main
  untouched and a re-run recovers cleanly; it will not double-publish.
- The Homebrew tap bump no-ops when `HOMEBREW_TAP_DISPATCH_TOKEN` is
  absent — a missing tap update with a green run usually means that.
