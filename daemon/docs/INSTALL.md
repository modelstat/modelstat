# Install modelstat

**One command. About 30 seconds. No account signup, no Node, no Docker.**

modelstat watches the local logs your AI tools already write (Claude Code, Cursor,
and friends), summarises each session **on your machine**, and streams the cleaned
result to your dashboard so you can see tokens, cost, and what you actually worked
on. Prompts, responses, file contents, and secrets never leave your machine in
readable form.

---

## Contents

- [Quick install](#quick-install)
- [What happens when you run it](#what-happens-when-you-run-it)
- [Requirements](#requirements)
- [Install per operating system](#install-per-operating-system)
  - [macOS](#macos)
  - [Linux](#linux)
  - [Windows](#windows)
  - [Servers & headless machines](#servers--headless-machines)
- [Summariser modes](#summariser-modes)
  - [Cloud (default)](#cloud-default)
  - [Local (beta)](#local)
  - [Self-hosted](#self-hosted)
  - [Choosing a mode during install](#choosing-a-mode-during-install)
  - [Changing mode later](#changing-mode-later)
- [Installer flags](#installer-flags)
- [Everyday commands](#everyday-commands)
- [Run your own summariser engine](#run-your-own-summariser-engine-self-hosted)
- [Updating](#updating)
- [Uninstalling](#uninstalling)
  - [Keep your pairing](#keep-your-pairing-stop-the-service-leave-the-data)
  - [Remove everything](#remove-everything)
- [Data & privacy](#data--privacy)
- [Where things live on disk](#where-things-live-on-disk)
- [Troubleshooting & FAQ](#troubleshooting--faq)

---

## Quick install

**macOS / Linux**

```sh
curl -fsSL https://modelstat.ai/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://modelstat.ai/install.ps1 | iex
```

That is the whole thing. It downloads a single small static binary, verifies it,
pairs your device, and opens your dashboard. There is nothing else to configure to
get started — the default (cloud summarising) needs no extra download.

---

## What happens when you run it

The installer prints every step as it goes. In order:

1. **Downloads the binaries.** Two static executables — the collector (`modelstat`)
   and the summariser engine (`modelstat-summarizer`) — are pulled from GitHub
   Releases and their SHA-256 checksums are verified before anything runs. No
   Node.js, no Python, no package manager.
2. **Replaces any older install.** If a previous modelstat daemon (including the
   old Node version) is running, it is stopped first and cleanly replaced. Your
   device pairing in `~/.modelstat` is kept, so you stay the **same device** on the
   dashboard.
3. **Puts `modelstat` on your PATH.** The binaries are installed to
   `~/.modelstat/bin`, and that directory is added to your shell startup file
   (`.zshrc`, `.bashrc`/`.bash_profile`, `.profile`, or a `conf.d` drop-in for
   fish) via a small `~/.modelstat/env` snippet. On Windows it is added to your
   user `Path` variable instead. A `--system` install symlinks into
   `/usr/local/bin` and touches no shell files. **A shell reads that file once at
   startup, so open a new terminal — or run `source ~/.modelstat/env` — before
   typing `modelstat`.** The installer prints exactly which files it changed.
4. **Registers your device** with modelstat.ai and gets a claim link.
5. **Asks where sessions should be summarised** — cloud, local, or self-hosted (see
   [Summariser modes](#summariser-modes)). This is a one-time consent choice.
   Redaction of secrets and names always runs on your machine first, in every mode.
6. **(local mode only — beta) downloads the summariser model** — a ~2.7 GB model
   (Qwen3.5-4B) so summarising can happen entirely on your machine.
7. **Prepares the on-device redactor** — a ~430 MB privacy model that finds and
   removes names/PII locally. Downloads in every mode.
8. **Prepares the on-device embedder** — a ~130 MB model (~560 MB with the
   redactor) that spots where the *topic* of your work changes, so a long session
   is split where the work actually changed rather than on elapsed time alone.
   Downloads in every mode.
9. **Installs a background service** (launchd on macOS, systemd `--user` on Linux, a
   Scheduled Task on Windows) so the daemon keeps running and survives reboots.
10. **(macOS) installs a menu-bar tray** icon.
11. **Enables the Claude Code statusline** — live tokens · cost · taxonomy in your
    terminal (skip with `MODELSTAT_NO_STATUSLINE=1`).
12. **Detects your AI tools** and wires the modelstat MCP into them, so you can ask
    your assistant about your own spend.
13. **Opens your dashboard** in the browser to claim the device.

A model download that fails is retried with backoff, and the daemon re-checks both
models every time it starts — so a drop-out mid-install repairs itself in the
background rather than leaving a model permanently missing. `modelstat status`
shows which ones are present.

Your data starts appearing on the dashboard within seconds.

---

## Requirements

| OS | Supported |
|---|---|
| **macOS** | 13 (Ventura) or newer — Apple Silicon or Intel |
| **Linux** | glibc, x86_64 or arm64, with `systemd --user` support |
| **Windows** | 10 or 11 — x86_64 or arm64 |

- **No runtime dependencies.** The binaries are self-contained. You do **not** need
  Node, Python, or anything else installed. (This is a change from the old
  installer, which bootstrapped Node.)
- **No root / admin** for a normal install — everything goes under your home
  directory and runs as a per-user service. Use `--system` only if you want a
  machine-wide service (that one needs admin).
- **Disk:** ~50 MB for the binaries + ~560 MB for the two on-device models
  (~430 MB redactor, ~130 MB embedder). Add ~2.7 GB only if you choose **local**
  summarising.

---

## Install per operating system

### macOS

```sh
curl -fsSL https://modelstat.ai/install.sh | sh
```

- Installs a **launchd** user agent, so the daemon starts at login.
- Adds a **menu-bar tray** icon for status at a glance.
- Everything lives under `~/.modelstat`. No `sudo` required.

### Linux

```sh
curl -fsSL https://modelstat.ai/install.sh | sh
```

- Installs a **systemd `--user`** service (`modelstat.service`).
- Requires user lingering to run when you are not logged in:
  `loginctl enable-linger "$USER"`.
- Everything lives under `~/.modelstat`. No `sudo` required.

### Windows

```powershell
irm https://modelstat.ai/install.ps1 | iex
```

- Installs a **Scheduled Task** named `modelstat` that runs at logon.
- Everything lives under `%USERPROFILE%\.modelstat`. No admin required.

Pass flags after the piped command like this:

```powershell
& ([scriptblock]::Create((irm https://modelstat.ai/install.ps1))) -Mode local
```

### Servers & headless machines

On a machine with no browser / no interactive terminal, tell the installer
everything up front so it never has to prompt:

```sh
curl -fsSL https://modelstat.ai/install.sh | sh -s -- --mode cloud --yes --no-browser
```

- `--mode` is **required** on a fresh non-interactive install (there is no silent
  default — you must state your consent choice once).
- `--yes` accepts the defaults; `--no-browser` skips opening the dashboard.
- Add `--system` to install a machine-wide service (needs root/admin).

---

## Summariser modes

Every mode runs **redaction on your machine first** — secrets and names/PII are
stripped locally before anything is sent anywhere. What differs is only *where the
summary itself is produced*.

### Cloud (default)

> modelstat's servers summarise your cleaned, redacted turns.

- **Resource:** no local model; negligible RAM, CPU, and battery on your machine.
- **Privacy:** your cleaned, redacted turns are uploaded and summarised
  server-side.
- **Best for:** most people. Nothing extra to install; lowest local footprint.

### Local

> A bundled model summarises entirely on **this** machine.

> ⚠ **Beta** — on-device summarising isn't validated end-to-end yet; **cloud** is the recommended default.

- **Resource:** ⚠ downloads a ~2.7 GB model (Qwen3.5-4B) and uses ~4 GB RAM plus
  extra battery/CPU while summarising.
- **Privacy:** only a short (≤240-character) abstract is uploaded — the raw turns
  never leave your machine.
- **Best for:** people who want the strongest privacy and have the RAM/disk to
  spare.

### Self-hosted

> Your organisation's own summariser engine, at a URL you provide.

- **Resource:** nothing extra here — summarising runs on the engine URL you point
  at.
- **Privacy:** only the abstract reaches modelstat; the cleaned excerpts go to your
  org's engine, not ours.
- **Best for:** teams that want to keep summarisation inside their own
  infrastructure. See [Run your own summariser
  engine](#run-your-own-summariser-engine-self-hosted).

### Choosing a mode during install

```sh
# Cloud (default — you can also just omit --mode when interactive)
curl -fsSL https://modelstat.ai/install.sh | sh -s -- --mode cloud

# Local
curl -fsSL https://modelstat.ai/install.sh | sh -s -- --mode local

# Self-hosted (point at your org's engine)
curl -fsSL https://modelstat.ai/install.sh | sh -s -- --mode self-hosted --url http://llm.acme.internal:4321
```

On Windows use `-Mode` / `-Url`.

### Changing mode later

```sh
modelstat mode                 # show the current mode
modelstat mode local           # switch to local (downloads the model, arms the engine)
modelstat mode cloud           # switch back to cloud (removes the local engine, keeps model files)
modelstat mode self-hosted --url http://llm.acme.internal:4321
```

Switching mode reconfigures the local engine, refreshes the background service, and
takes effect on the next scan. You can also pin a mode with the environment variable
`MODELSTAT_SUMMARIZER_MODE`, or a self-hosted URL with `MODELSTAT_SUMMARIZER_URL`;
when set, these override the stored choice.

---

## Installer flags

Pass flags after `| sh -s --` on macOS/Linux, or directly on Windows.

| Flag (Unix) | Flag (Windows) | Meaning |
|---|---|---|
| `--component daemon\|summarizer` | `-Component` | Install the collector (default) or a standalone summariser engine. |
| `--user` *(default)* / `--system` | `-System` | Per-user install, or a machine-wide service (needs root/admin). |
| `--version X.Y.Z` | `-Version` | Install a specific release instead of the latest. |
| `--mode cloud\|local\|self-hosted` | `-Mode` | Summariser mode (required on a fresh non-interactive install). |
| `--url <URL>` | `-Url` | Engine URL for `self-hosted` mode. |
| `--yes` / `-y` | `-Yes` | Accept defaults; don't prompt. |
| `--no-browser` | `-NoBrowser` | Don't open the dashboard at the end. |
| `--no-auto-update` | `-NoAutoUpdate` | Don't let the daemon update itself automatically. |

**Auto-update** is on by default (it is how you receive fixes). It is turned off
automatically if you install a **pre-release** build, or if you pass
`--no-auto-update`. Toggle it any time with `modelstat autoupdate on|off`.

---

## Everyday commands

Run these in any terminal after install:

| Command | What it does |
|---|---|
| `modelstat` | Re-run onboarding (safe to repeat — idempotent). Aliases: `connect`, `reinstall`. |
| `modelstat status` | Pairing, service state, and a live usage snapshot. Add `--json` for machines. |
| `modelstat jobs` | The local pipeline queue and recent activity. |
| `modelstat mode` | Show or change where sessions summarise. |
| `modelstat discover` | List the AI tools and signed-in accounts detected locally (read-only). |
| `modelstat sync --session <id> [--wait]` | Force one session to be scanned right now. |
| `modelstat reset` | Re-read and re-summarise **everything** from scratch (wipes local cursors). |
| `modelstat upgrade` | Update to the latest release now. |
| `modelstat autoupdate on\|off\|toggle` | Set the auto-update preference. |
| `modelstat stop` | Remove the background service (your pairing is kept). Aliases: `remove`, `uninstall`. |
| `modelstat paths` | Show resolved file paths (state, identity, logs, API). |
| `modelstat token` | Print this device's bearer token (treat it like a password). |

---

## Run your own summariser engine (self-hosted)

For a team that wants summarisation to happen on its **own** box, install the
engine component on a server and point your collectors at it.

**On the engine server:**

```sh
curl -fsSL https://modelstat.ai/install.sh | sh -s -- --component summarizer
```

This runs `modelstat-summarizer setup`, which:

- asks for a **bind address** and **port** (binding to `0.0.0.0` requires typing
  `expose` to confirm — the engine has **no authentication**, so only do that
  behind a firewall or reverse proxy you trust),
- downloads the model,
- installs the engine as a background service,
- asks whether to enable daily auto-update (default: off, so a shared box changes
  predictably),
- and prints the exact command your collectors should run.

**On each collector machine**, point it at the engine:

```sh
modelstat mode self-hosted --url http://<engine-host>:<port>
```

Engine management commands (on the server):

| Command | What it does |
|---|---|
| `modelstat-summarizer status` | Show config and probe the running engine. |
| `modelstat-summarizer serve` | Run the engine in the foreground. |
| `modelstat-summarizer stop` | Stop the engine service (keeps it installed). |
| `modelstat-summarizer uninstall` | Remove the engine service (keeps model files). |
| `modelstat-summarizer upgrade` | Update the engine to the latest release. |

---

## Updating

- **Automatic (default):** the daemon checks for new releases and updates itself in
  the background. Nothing to do.
- **Manual:** `modelstat upgrade`.
- **Turn auto-update off/on:** `modelstat autoupdate off` / `modelstat autoupdate on`.

Updates are verified by SHA-256 (and a signature when signing is enabled), applied
by swapping the binaries, health-checked, and rolled back automatically if the new
version fails to come up.

---

## Uninstalling

### Keep your pairing (stop the service, leave the data)

```sh
modelstat stop        # or: modelstat remove  /  modelstat uninstall
```

This removes the background service, the menu-bar tray, the Claude Code
statusline, and the PATH entry from your shell startup file. Your device pairing
is **kept** in `~/.modelstat`, so running `~/.modelstat/bin/modelstat` (full path
— the PATH entry is gone) re-enables everything later.

To reclaim disk without fully uninstalling, delete just the models:
`rm -rf ~/.modelstat/models/`. Note this frees space only while the daemon is
stopped — a running daemon re-downloads any missing model on its next start.

### Remove everything

```sh
curl -fsSL https://modelstat.ai/uninstall.sh | sh
```

```powershell
irm https://modelstat.ai/uninstall.ps1 | iex
```

Total removal. Afterwards the machine is indistinguishable from one that never
installed modelstat. It covers **both eras in one pass** — the native Rust
install and the retired npm/Node one — because a machine that upgraded through
the npm era can still be carrying a global `modelstat` that shadows the native
binary on PATH, plus an `npx @modelstat/mcp` entry in every AI tool.

It removes:

| | |
|---|---|
| Services | all three launchd agents / both systemd units, per-user and system scope, plus Windows scheduled tasks and services |
| Processes | anything of ours still running, including the tray |
| Binaries | `~/.modelstat/bin`, the `/usr/local/bin` symlinks, the Homebrew formula and tap |
| Packages | `modelstat`, `@modelstat/daemon`, `@modelstat/mcp` across npm, pnpm, yarn, bun and volta — and, because those tools cannot always find their own packages (see below), a direct sweep of every global root on disk |
| Launchers | every `modelstat` / `modelstat-summarizer` command, including under Node versions that are not active right now |
| Caches | npx and pnpm-dlx copies |
| Wiring | MCP entries in 13 config locations plus Codex TOML and the `claude` CLI; the Claude Code statusline and plugin registration |
| PATH | our block in every shell startup file, the fish drop-in, and Windows Path entries |
| Data | `~/.modelstat` and `/var/lib/modelstat` in full — identity, models, logs |

**Nothing is preserved, including your device identity.** Installing again
registers the machine as a **brand-new device** on the dashboard. There is no
flag to keep the old pairing — use `modelstat stop` above if that is what you
want.

It leaves alone: Node.js and your package managers, other MCP servers in the
same config files, a statusline of your own (restored if we had composed over
it), and Claude Code's session history in `~/.claude/projects/` — those
directories are named after *your* repo paths, so some contain the word
"modelstat" without being ours.

Pass `--dry-run` (`-DryRun` on Windows) to print every change without making
one. Config edits are written atomically and leave no `.bak` files behind. A
system-wide install needs `sudo` / an elevated PowerShell; the script names
exactly what is left if you are not elevated.

> **Why it does not just ask the package manager.** pnpm resolves `-g` against
> the *current* store version, so a package installed by an older pnpm
> (`~/Library/pnpm/global/5`) is invisible to a newer one that resolves to
> `global/v11`: `pnpm ls -g` reports "No global packages found" and
> `pnpm remove -g` silently does nothing — while the command is still on PATH
> and working. nvm, fnm and volta have the same shape, one global root per Node
> version. So the uninstaller asks every manager *and* sweeps the roots on disk,
> and it removes launcher shims separately from packages, because removing a
> package does not always remove its launcher.

---

## Data & privacy

- **Redaction is always local.** Secrets and names/PII are removed on your machine
  before anything is sent, in every mode.
- **Prompts, responses, file contents, and secrets stay on your machine.** In cloud
  mode, only *cleaned, redacted* turns are uploaded. In local and self-hosted mode,
  only a short abstract leaves your machine.
- **Read-only access.** The collector only reads the log directories your AI tools
  already write to. It does not need root/sudo for a normal install.

---

## Where things live on disk

Everything is under one directory:

- **macOS / Linux:** `~/.modelstat`
- **Windows:** `%USERPROFILE%\.modelstat`
- **`--system` installs:** `/var/lib/modelstat` (Linux) or `%ProgramData%\modelstat`
  (Windows)

Inside it:

| Path | Contents |
|---|---|
| `bin/` | the `modelstat` and `modelstat-summarizer` binaries |
| `env` | the one-line PATH snippet your shell startup file sources |
| `models/` | downloaded models (redactor + embedder, and Qwen for local mode) |
| `identity.json` | your device pairing (keep this to stay the same device) |
| `state.json` | scan cursors and local state |
| `logs/` | daemon logs — `out.log` is what it did, `err.log` is what went wrong (see below) |
| `summarizer.json` | engine config (self-hosted / local engine only) |

---

## Troubleshooting & FAQ

**Nothing shows up on the dashboard.**
Run `modelstat status` to check pairing and the service, and `modelstat jobs` to see
the local queue. If the device is not claimed, open the claim link the installer
printed.

**Where are the logs, and which file do I open?**
`~/.modelstat/logs/`. The daemon splits its lines by severity:

| File | Holds |
|---|---|
| `out.log` | routine progress — what the daemon did, and when |
| `err.log` | warnings and errors only — start here when something is broken |

The summariser engine writes the same pair as `summarizer-out.log` /
`summarizer-err.log`, and the menu-bar tray as `tray-out.log` / `tray-err.log`.
Every supervised line starts with a UTC timestamp, so to follow one incident
across both files, read them together in time order:

```bash
sort ~/.modelstat/logs/out.log ~/.modelstat/logs/err.log | tail -50
```

A log that passes 64 MB is trimmed on the next daemon start, with the tail kept
in the matching `.old.log`.

**`modelstat: command not found` right after installing.**
Your current shell read its startup file before the installer edited it. Open a
new terminal, or run `source ~/.modelstat/env`. If it still isn't found, check
that your startup file (`.zshrc`, `.bashrc`, `.bash_profile`, or `.profile`)
contains the `modelstat` line the installer printed — and remember the full path
`~/.modelstat/bin/modelstat` always works.

**`modelstat` runs an old version.**
Another copy is earlier on your PATH — most often a global npm/pnpm/yarn/bun
install of the retired Node package. Check with `command -v modelstat` (Unix) or
`Get-Command modelstat` (Windows); remove that one, then open a new terminal.

**I want to re-process my history from scratch.**
`modelstat reset` wipes the local cursors so the next scan re-reads and
re-summarises everything.

**How do I check the on-device models actually downloaded?**
`modelstat status` lists them under **on-device models**, with `✓` or `✗` per
model (`--json` puts the same thing under `models`). A missing one is not fatal —
the daemon keeps retrying in the background — but until the embedder lands,
sessions are split on time and size alone, and in cloud or self-hosted mode a
missing redactor holds uploads entirely rather than sending unredacted text.

**Does it need Node / Python / Docker?**
No. The binaries are self-contained.

**Does it run as root?**
No, not for a normal install — it runs as a per-user service under your home
directory. `--system` is the only mode that installs a machine-wide service.

**How do I install a specific version?**
Pass `--version X.Y.Z` (Unix) or `-Version X.Y.Z` (Windows).

**Is my device the same after re-installing or upgrading?**
Yes. As long as `~/.modelstat` (specifically `identity.json`) is intact, you remain
the same device on the dashboard — no duplicate.

**Help / support:** https://modelstat.ai/install
