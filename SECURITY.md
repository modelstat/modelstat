# Security policy

modelstat takes the security of its daemon software seriously — this is
the code that watches files on your laptop and talks to the cloud, so it
has to be worth trusting.

## Reporting a vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Instead, either:

- Email **[security@modelstat.ai](mailto:security@modelstat.ai)** with a
  description of the issue, steps to reproduce, and any proof-of-concept
  you have.
- Or use GitHub's private vulnerability reporting:
  **[github.com/modelstat/modelstat/security/advisories/new](https://github.com/modelstat/modelstat/security/advisories/new)**

We try to acknowledge within **one business day** and aim to ship a fix
within **30 days** for high-severity issues. Critical vulnerabilities
(RCE, credential exfiltration, etc.) get same-day attention.

## Scope

**In scope** — anything in this repository, plus the published artifacts:

- `modelstat` (npm)
- `@modelstat/daemon-sdk` (npm)
- `@modelstat/mcp` (npm)
- The `modelstat-tray` macOS app
- `install.modelstat.ai` shell installer
- The Homebrew formula at `modelstat/homebrew-tap`

**Out of scope** — issues affecting the hosted service itself. Those
aren't in this repo; please report them to `security@modelstat.ai`
with a `[service]` tag so they're routed correctly.

## What we consider a security issue

- Code execution triggered by malformed tool log files (JSONL, SQLite,
  etc.) that the parsers ingest.
- Any path by which prompts, file contents, or credentials leak past
  the client-side redactor (`packages/daemon-sdk/`) into an upload.
- Privilege escalation or persistence abuse in the macOS tray launch
  daemon.
- Supply-chain risks in the npm-published artifacts — build-time
  script execution, typosquat shadowing, etc.

## What we don't consider a security issue

- Redaction patterns missing a new secret shape — that's a routine
  improvement. Open a PR adding the pattern to
  `packages/daemon-sdk/src/redact.ts` + a test case.
- The bearer token `npx modelstat@latest` writes to your local config
  directory. It's stored unencrypted on purpose — rotate it via
  `npx modelstat@latest` if you suspect it's been exposed. It's scoped
  to a single device × user.
- Policies we explicitly opt-in to (e.g. the `none` redaction policy
  is meant for trusted sandboxes).

## Hall of fame

We credit reporters of confirmed vulnerabilities (with permission) on
the changelog. No bounty program yet, but we're generous with swag and
reference letters.

## Signing

- The release workflow (`.github/workflows/release.yml`) publishes
  npm packages with provenance statements — see
  `modelstat`'s npm page for verification.

## Canonical contact

See **[/.well-known/security.txt](https://modelstat.ai/.well-known/security.txt)** on the
site for the machine-readable RFC 9116 version.
