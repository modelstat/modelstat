# AGENTS.md — working on the modelstat companion

Guidance for developers and coding agents in this repo. Keep it current:
when you change how something here works, update this file in the same PR.

## What this repo is

The modelstat **companion** — everything that runs on a user's machine and
feeds the server: the Node CLI/daemon (`packages/*`, published to npm +
Homebrew) and the macOS tray app (`apps/tray-mac`). The server
(ingest/pipeline/dashboard, modelstat.ai) is a separate private service
(closed-source) and is out of scope for this repo.

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
  SDK redactor (`@modelstat/daemon-sdk`) — add a newly-leaked credential format
  there, once. The server can *augment* it at runtime via a signed, additive
  `policies` config: the floor always applies and a signed bundle can only ever
  add patterns, never remove or weaken them.
- `@modelstat/remote-config` is the shared signed-config loader (fetch → verify
  Ed25519 over raw bytes → disk-cache under `~/.modelstat/config/` → fall back
  memory→disk→bundled). The long-lived daemon refreshes the `policies` kind on a
  timer; new server-delivered config kinds ride this loader instead of forcing a
  release.

## Releasing (npm + Homebrew)

Releases are **zero-touch**. Every push to main runs `release`
(`.github/workflows/release.yml`), which decides which publishable packages
changed and what version each gets, then publishes to npm, tags, bumps main,
and cuts a GitHub Release. No `release_type` input, no OTP, no two-phase split.

**How the version is chosen** — `.github/scripts/release-plan.mjs` reads the
Conventional Commits since each package's last tag (the **tag** is the source
of truth for the last released version, not `package.json`):

- `feat:` → minor · `fix:`/`perf:`/`refactor:`/`revert:` → patch ·
  `!`/`BREAKING CHANGE` → major. `chore`/`docs`/`ci`/`test`/`style`/`build`
  alone → **no release**.
- **Pre-1.0 clamp**: while a package is `0.x`, a breaking change bumps the
  minor (`0.1.3` → `0.2.0`), never auto-jumping to `1.0.0`.
- **Dependency-aware**: a package is "changed" if its own dir *or any of its
  transitive workspace deps* changed. A `fix(core):` in `packages/core`
  therefore republishes `modelstat` and `@modelstat/daemon-sdk` (both depend on
  it) but not `@modelstat/mcp` (no workspace deps).
- The publishable set is every workspace package with `private !== true`, so a
  new public package is picked up automatically. The tag prefix derives from the
  package's unscoped name (`modelstat` → `modelstat-v`, `@modelstat/mcp` →
  `mcp-v`); the only hand-maintained list is `SKIP_PUBLISH` (currently
  `@modelstat/daemon-sdk`, until its npm trusted publisher is configured).

**Auth — npm Trusted Publishing (OIDC), no token.** The runner mints a
short-lived OIDC token (`id-token: write`) that npm exchanges for a publish
credential, and every publish carries provenance. There is **no `NPM_TOKEN`**.
This requires a **one-time setup per package** on npmjs.com:

> Package → Settings → **Trusted Publisher** → GitHub Actions →
> org `modelstat`, repo `modelstat`, workflow `release.yml`.

A brand-new package that isn't on npm yet needs **one bootstrap publish** (a
manual `npm publish` from a maintainer, or org-level trusted publishing) before
the OIDC flow can take over; after that it's hands-off.

**Runners** — the daemon (`modelstat`) builds on macOS (it bakes a universal,
ad-hoc-signed `ModelstatTray.app` into its tarball, which needs full Xcode);
the pure-JS packages build on ubuntu. The plan step picks the runner per
package, so a merge that doesn't touch the daemon never spins a macOS runner.

To **skip** a release, use a non-releasing commit type (`chore:`, `docs:`, …).
To **force** one, merge a `fix:`/`feat:` that touches the package (or trigger
the workflow manually: GitHub → Actions → release → Run workflow).

### Observing a release

```sh
gh run list --workflow=release.yml --limit 3
gh run watch <run-id> --exit-status
```

The plan step prints a "Release plan" summary (what's shipping at what
version). Verify the artifact landed: `npm view modelstat version` and the
`<pkg>-v<version>` tag + GitHub Release exist.

### When a release fails

```sh
gh run view <run-id> --log-failed
```

The whole flow is **idempotent** — re-running (push an empty commit, or
re-run the job) converges:

- npm publish skips a version already on npm, so nothing double-publishes.
- The planner reads the **tag**, so if a run died after `npm publish` but
  before tagging, the re-run recomputes the same version, skips the (already
  published) npm step, and just finishes the tag/bump/release.
- `403` / "Trusted Publisher" errors → the per-package trusted publisher above
  isn't configured yet (or the package doesn't exist on npm — do the one
  bootstrap publish).
- The npm publish runs BEFORE anything touches main, so a failed publish leaves
  main untouched.
- The Homebrew tap bump no-ops when `HOMEBREW_TAP_DISPATCH_TOKEN` is
  absent — a missing tap update with a green run usually means that.
