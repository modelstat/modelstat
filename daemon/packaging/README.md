# Distribution packaging (M7)

Staged here in the daemon repo (the source of truth); each file is deployed to
its own home at cutover (M9). See feature §3/§13/§22 and plan §8/D9.

| File | Deploys to | Purpose |
|---|---|---|
| [`../scripts/install.sh`](../scripts/install.sh) | served at `modelstat.ai/install.sh` (core-repo Caddyfile rule, 302 → this file on GitHub) | `curl \| sh` — download + verify + stage + `connect` |
| [`../scripts/install.ps1`](../scripts/install.ps1) | served at `modelstat.ai/install.ps1` (core-repo Caddyfile rule, 302 → this file on GitHub) | `irm \| iex` Windows installer |
| [`../scripts/uninstall.sh`](../scripts/uninstall.sh) | served at `modelstat.ai/uninstall.sh` (core-repo Caddyfile rule, 302 → this file on GitHub) | `curl \| sh` — TOTAL removal: both the native install and the retired npm/Node one (services, globals, launchers, MCP, statusline, plugin, PATH, `~/.modelstat`) |
| [`../scripts/uninstall.ps1`](../scripts/uninstall.ps1) | served at `modelstat.ai/uninstall.ps1` (core-repo Caddyfile rule, 302 → this file on GitHub) | `irm \| iex` Windows equivalent |
| [`../../.github/workflows/release-daemon-rs.yml`](../../.github/workflows/release-daemon-rs.yml) | already in place | tag `daemon-<semver>` → build 6 targets → checksum/sign → GitHub Release |
| [`homebrew/modelstat.rb`](homebrew/modelstat.rb) | tap repo `modelstat/homebrew-tap` → `Formula/modelstat.rb` | binary formula (installs both binaries) |
| [`homebrew/bump-formula.yml`](homebrew/bump-formula.yml) | tap repo → `.github/workflows/bump-formula.yml` | the missing `agent-released` listener that bumps the formula per release |
| [`npm-deprecation/`](npm-deprecation/) | published as the FINAL `modelstat` npm version (M9) | postinstall bridges stranded Node daemons onto the native installer |

## Release → distribution flow

1. Push a `daemon-<semver>` tag → `release-daemon-rs.yml` builds both binaries for
   all six targets, bakes the prebuilt macOS tray in, writes `SHA256SUMS`
   (+ `SHA256SUMS.minisig` when `MINISIGN_SECRET_KEY` is set), and publishes the
   GitHub Release. The installer + the binary self-updater consume these assets.
2. The release job should also fire an `agent-released` `repository_dispatch` at
   the tap repo so `bump-formula.yml` refreshes the Homebrew formula. (Wire this
   dispatch when the tap repo + token exist — M9 ops.)

## Not done here (environment-gated — need real infrastructure)

- **Real signing**: `MINISIGN_SECRET_KEY` + the Apple Developer ID cert /
  notary key are repo secrets; the workflow steps are written to run *when they
  exist* and no-op otherwise.
- **Live e2e**: `curl \| sh` / `irm \| iex` on fresh VMs and the
  staging→staging self-update swap+rollback (plan §5 M7 AC) need a real published
  release to download — run once the first tag ships.
- **Serving + Caddyfile**: pointing `modelstat.ai/install.sh` (+ `/install.ps1`,
  `/uninstall.sh`, `/uninstall.ps1`) at these scripts is a core-repo ops step
  (plan §8). Until those rules land, the uninstaller is reachable at its raw
  GitHub URL — see [`../scripts/uninstall.sh`](../scripts/uninstall.sh).
