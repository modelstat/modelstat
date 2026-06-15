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

Releases are **manual** and **OTP-gated** — a live 2FA code from the
maintainer's authenticator is required for every npm publish, so a leaked
`NPM_TOKEN` alone can't ship a package. It's a two-phase flow: build is slow,
but the publish must land inside the OTP's ~30s window, so they're separate
runs.

**1. Build** (`.github/workflows/release-build.yml`) — bumps `package.json`,
builds, and packs a tarball artifact. Publishes nothing; touches neither npm
nor main. The run summary prints the run id you need for phase 2.

```sh
gh workflow run release-build.yml -f package=agent -f release_type=patch
# package: agent | mcp;  release_type: patch | minor | major | none
# or -f version=X.Y.Z to pin an exact version
```

**2. Publish** (`.github/workflows/release-publish.yml`) — downloads that
tarball and runs `npm publish … --otp=<code>` as its first step, then commits
the bump to main → tags `<pkg>-v<version>` → GitHub Release → (agent) Homebrew
tap bump.

```sh
gh workflow run release-publish.yml -f build_run_id=<id> -f otp=<fresh-code>
```

(or GitHub → Actions → the two "Release · …" workflows → Run workflow.)

### Observing a release

```sh
gh run list --workflow=release-publish.yml --limit 3
gh run watch <run-id> --exit-status
```

Verify the artifact landed: `npm view modelstat version` (agent) and the
GitHub Release/tag exist.

### When a release fails

```sh
gh run view <run-id> --log-failed
```

- npm `EOTP` → the code expired before the publish step ran (a slow runner).
  Just re-run phase 2 with a fresh code — publish is idempotent (it skips a
  version already on npm), so nothing double-publishes and main stays clean.
- npm `ENEEDAUTH` / `E403` → `NPM_TOKEN` is missing or lacks publish rights:
  `gh secret set NPM_TOKEN`. It does **not** need to be an Automation token —
  any publish-capable token plus the OTP is enough.
- The publish runs BEFORE anything touches main, so a failed publish leaves
  main untouched.
- The Homebrew tap bump no-ops when `HOMEBREW_TAP_DISPATCH_TOKEN` is
  absent — a missing tap update with a green run usually means that.
