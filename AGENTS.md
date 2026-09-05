# AGENTS.md — working on the modelstat daemon

Guidance for developers and coding agents in this repo. Keep it current:
when you change how something here works, update this file in the same PR.

## CRITICAL — read first (repeated at the end)

Nobody uses this service yet — there's no data or behaviour to preserve. Every change lands as the **final, canonical version**, as if the code had always been written that way:

- **Fix the root cause, in the right place** — no hacks, workarounds, or symptom patches.
- **Delete legacy/outdated code in the same change** — migrate every caller and remove the old path; prefer the clean breaking change over the cautious additive one.
- **Leave zero cruft** — no back-compat shims, aliases, deprecated paths, dead flags, `_v1`/`_v2` forks, or commented-out code.
- **Only ordering to mind** — the daemon (npm, auto-updates) and server can't cut over atomically, so change both sides and bump; that's an ordering note, not a license for a permanent shim.

## What this repo is

The modelstat **daemon** — everything that runs on a user's machine and feeds
the server: the native Rust daemon in `daemon/` (what `install.sh` installs and
what ships), the macOS tray app (`apps/tray-mac`), the MCP server
(`packages/mcp`), and the standalone SDKs (`sdks/*`). The TypeScript daemon is
retired and its tree is deleted; `packages/core` remains as the wire/redaction
reference the golden fixtures are generated from, and `packages/daemon-core`
keeps only the segmentation pipeline the Chrome extension ships — never publish
a package named `modelstat` from this repo.
 The server (ingest/pipeline/dashboard,
modelstat.ai) is a separate private service (closed-source) and is out of scope
for this repo.

## Design principle: the weakest sufficient hypothesis

Among all designs that **exactly fit the cases actually observed**, prefer the
one that commits to the least beyond them — it is the most likely to survive
unseen tools, schemas, and formats. (Bennett, AGI-23,
[arXiv:2301.12987](https://arxiv.org/abs/2301.12987): a hypothesis generalises
in proportion to its *extension* — how much it stays correct for — not its
brevity. The theorem is stronger than a preference: maximising weakness is
NECESSARY AND SUFFICIENT to maximise the probability a hypothesis generalises,
shortness is neither, and in the paper's experiments the weakest generalised at
1.1–5× the rate of the shortest. A compact rule can be maximally
overcommitted.)

This repo already runs on it — keep new code on the same line:

- **Parsers emit raw events verbatim and never interpret** — the server
  decides. An interpretation baked into the daemon is a commitment about every
  future version of every tool.
- **Discovery probes by artefact shape** (a directory that *looks like* agent
  data), never by app-name or install-path allowlists — new and relocated
  tools are found without a release.
- **Discovery also reports the machine's PERSON handles** — the gh CLI's
  signed-in logins (`hosts.yml`, never its tokens) and the global git identity
  (email + name) — as `handles` beside `identities` on the discovery heartbeat
  (`modelstat discover` shows them). A handle is a fact about whoever paired
  the device; the server folds it onto their person profile. `provider` is an
  open slug (`github`, a GitHub Enterprise host, `email`, …), never an enum.
- **No allowlists for open-ended sets found in the data** (model names, tool
  names, metadata categories) — pass strings through; bounded rosters exist
  only where code must exist per case (e.g. the parser set).
- **The flip side — known contracts are commitments to keep, made explicitly:**
  the redaction floor and the wire schema are deliberate, validated, and live
  irreducibly where they're enforced; they are data about the world, not
  overreach. And a weak design still has to decide every observed case
  exactly — fit all real fixtures.

Review test: *what unseen-but-plausible input would this code silently
mishandle, and what in today's data forces that commitment?* If nothing forces
it, weaken the design — usually by deleting structure (a pass-through, a
shape-probe, a string) rather than adding speculative abstraction.

## Never fail silently, never degrade silently — HARD RULE

Code either **works**, or **fails loudly with a clear, specific reason**. There
is no third option. A silent degrade is worse than a crash: nothing alerts,
nothing is counted, and the wrong answer looks exactly like the right one.

Banned outright:

- **A fallback that invents a value to replace an honest "I don't know."**
  `dominant.unwrap_or_else(|| "unknown".into())` is the canonical bug: the code
  computes a correct `None` and throws it away one line later, so a magic string
  lands in the same column as real values and every reader downstream has to
  guess which is which. Absence must stay absence all the way through. (This is
  also why a parser writes what it OBSERVED and never a stand-in — the daemon
  never drops data, and it never invents any either.)
- **A `continue` / early-out that drops a record with no log and no counter.**
- **A `catch` / `match` arm that discards an error and carries on.**
- **A default standing in for a missing REQUIRED input.** Reject it instead.
- **Partial success reported as success.**

Required instead:

- **Reject at the door**, with an error naming what was missing and what to send.
  Half a key is not a key. The door is the one place that knows without guessing,
  so the guard goes there — not in each caller.
- **Count every skip, and surface the count in the operation's own result** — not
  only in a log line. A run that found nothing and a run that could never have
  found anything must read differently.
- **Say which gate closed.** "Skipped 7 records: no timestamp" is actionable;
  "skipped 7" is not. The `SkipLedger` in the parsers exists for exactly this.

Review test for any fallback you write: *what does a reader learn when this
fires?* If the answer is "nothing", it is a silent degrade — turn it into a loud
failure or a counted, surfaced skip.

## Naming: daemon, not agent/companion

Our local long-running process is the **daemon** (what `curl -fsSL https://modelstat.ai/install.sh | sh` installs; the native `modelstat` binary built from `daemon/`). Never call it "agent" or "companion".

- **daemon** — our process / CLI / SDK side. (It was historically "companion", and earlier "agent" — both retired.) Use `daemon` for routes (`/v1/daemon/heartbeat`), env (`DAEMON_API_URL`), the launchd label (`ai.modelstat.daemon`), `packages/daemon-core`, the `daemon_version` wire field, etc.
- **agent** — ONLY the user's AI tool (`claude_code`, `cursor`, `codex_cli`, …). Keep "agent" for: the `AGENTS` enum + the `agent` event field, the `/device/:claim/agent` machine-readable view (it's *for* AI agents/LLMs), the `User-Agent` header, this `AGENTS.md`, and "agentic".
- **companion** — retired; don't reintroduce.

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
- Parsers (`daemon/crates/modelstat-parsers`) emit raw events only and keep transcript
  data verbatim — e.g. the `<synthetic>` pseudo-model Claude Code records
  for local error/notice messages is passed through as-is (the server
  decides what to hide; the daemon never drops data). The one exception:
  `<synthetic>` must not update the parser's `lastModel` attribution state.
- **Every event carries `seq`** — its 1-based position in the source log it was
  read from (the line ordinal in a transcript, the record ordinal within a
  conversation for Cursor's key/value store). `ts` cannot order a log: parsers
  routinely see runs of records sharing one millisecond. Two properties make it
  worth carrying, and both are load-bearing when you touch a parser:
  - it counts SOURCE RECORDS, not emitted events, so a line this build drops
    still costs its position and a line a future build starts reading does not
    renumber its neighbours;
  - it is stable across scans, because a positional parser always reads its file
    from the top (the upload cursor gates the SEND, never the READ) and Cursor
    counts in its own total order BEFORE the since-floor applies.
- **A repo slug must carry its `slug_source` provenance.** Every producer of a
  `GitContext.remote_slug` states how it reached the slug (`git_remote`,
  `repo_root_dir`, `path_shape`); unstated provenance is treated as a GUESS —
  downstream (`slug_is_verified`, the `projects` hint tiering, the
  `git`/`git_guess` repo-ref split) never takes an unmarked slug on faith. The
  one exception is evidence: a context carrying a real `remote_url` is verified
  even without the marker, because no guess path has ever written one.
- **The device's time zone** is stated by `modelstat-ingest::timezone`: the IANA
  NAME plus the current UTC offset ride the heartbeat, and the offset alone is
  stamped on every `IngestBatch` **at build time** (not at upload — a spooled
  batch belongs to the offset its events were gathered under). Everything else on
  the wire is UTC, so this is the only way anything downstream can tell 09:00 in
  one zone from 09:00 seven hours away.
- **The SDKs state their own call instants.** `RawEvent.started_at` /
  `first_token_at` are optional and sit BESIDE `ts`, which is unchanged. Only a
  producer inside the call path can state them, so the parsers always omit them;
  `wrap()` reads the clock before it forwards the request (recording happens
  after the response, so a call built there would date itself to the wrong end),
  and it leaves `first_token_at` unset because it sees one whole response rather
  than a first chunk. A caller who streams sets it on their own `LlmCall`.
- **Summariser mode** is chosen at install (`modelstat connect`, Cloud
  pre-selected) and persisted to `state.json` (`summarizerMode`; env override
  `MODELSTAT_SUMMARIZER_MODE`). The install chooser (`MODE_INFO` in `cli.ts`)
  states each mode's RESOURCE + PRIVACY profile — including the RAM/battery
  warning on local. The active mode is surfaced and changeable after install
  via `modelstat mode`, `modelstat status` (+ the `summarizer` object in
  `status --json`), and the macOS tray's **Summariser** submenu (one-click
  cloud/local switch; self-hosted needs a URL+model, so it points at the CLI).
  Redaction (regex floor + on-device NER/PII) runs client-side in EVERY mode;
  only the summarisation LOCATION differs — see `Config::summarizer_mode` in
  `daemon/crates/modelstat-ingest/src/config.rs` and the engine selection in
  `daemon/crates/modelstat-daemon/src/engine.rs`:
  - `local` — the bundled Qwen GGUF via `node-llama-cpp`, staged by
    `installNativeRuntime`/`_setup-runtime`. The **only** mode that
    downloads/stages the ~2.7 GB model (`installNativeRuntime` stages
    `node-llama-cpp` only in local mode; `connect`/postinstall gate the model
    pull on it too). Ships abstracts to `/v1/ingest`.
  - `self-hosted` — an org-run OpenAI-compatible endpoint (URL + model chosen at
    install, or `MODELSTAT_LLM_BASE_URL`/`_MODEL`/`_API_KEY`); see
    `openai-compat.ts` (`makeOpenAICompatConfig`). Explicit egress: excerpts +
    script bodies leave the box, but only after the on-device pre-send scrub
    (regex floor + NER/PII, `makeRemotePreSend`); embeddings + output PII
    redactor stay local. Ships abstracts to `/v1/ingest`.
  - `cloud` (default) — no local model; the daemon runs the full redaction over
    the turns (`prepareCloudRawEvents`: floor + NER) and ships them to
    `/v1/ingest/raw` for server-side summarisation (see `scan.ts`).

  If the selected runtime can't run (native binary won't load, self-hosted
  endpoint misconfigured, NER redactor unavailable), the pipeline degrades
  LOUDLY to the dependency-free extractive fallback (`resilientSummarize` /
  `heuristicSummarize`) so ingest never blocks. The self-hosted and cloud paths
  are **fail-closed** on the NER guard — if the on-device NER redactor
  (`@huggingface/transformers`) is a silent pass-through, the daemon REFUSES the
  egress (self-hosted → extractive fallback; cloud → local extractive abstracts
  to `/v1/ingest`, no raw egress) rather than ship content with
  regex-floor-only redaction. Remote requests retry with bounded backoff
  (honouring `Retry-After`) and send `max_completion_tokens` (no `temperature`)
  to o-series/gpt-5 reasoning models. The legacy `ollama.ts` adapter remains
  exported but is unwired in the daemon.
- Redaction has **one floor**, and it is compiled in. In the Rust daemon it is
  `modelstat-redact` (`floor.rs` is the catalogue — add a newly-leaked credential
  format there, once); the TS line's copy lives in `@modelstat/core/redact-floor`
  and the standalone `@modelstat/sdk` keeps its own. The server can *augment* it
  at runtime via the additive `policies` config: the floor always applies, and a
  bundle can only ever add patterns, never remove or weaken them. The compiled
  augment is installed process-wide in `modelstat-redact` rather than passed to
  each caller, so every floor call site gets it and no new one can miss it by
  omission.
- **Server-delivered config** rides `modelstat-ingest::remote_config`: fetch
  `GET {api}/v1/config/{kind}` → shape-validate with the kind's own validator →
  version-gate (strictly newer only, so nothing can roll a device back) →
  disk-cache under `~/.modelstat/config/` → resolve memory → disk → compiled-in
  default. The daemon refreshes every kind at boot and every 6h; a new kind is a
  validator plus one install line, and needs no change to the channel.
  **Trust is the TLS connection to the api origin — there is no payload
  signature and no request signing.** What makes a bad payload harmless is the
  shape of each kind (`policies` can only ADD redaction; `calibration`'s values
  are clamped to a tenth-to-ten-times their compiled defaults), not a key.
  Ed25519 was specified for this on the TS line and deliberately dropped; don't
  reintroduce it.

## Test fixtures & examples — ALWAYS fictional

This is a **public** repo. Every example, test fixture, sample command, comment
snippet, and doc must be **fictional**. Never paste anything real — not even "a
redaction of" real data, and not even a value you believe is dead or harmless.

- **Secrets** — never a real key/token/password. Synthesize a value that matches
  only the *format* (`sk_live_…`, `1000000000:…`, `phc_…`). The redactor floor
  keys on the variable **name** and length, not the literal value, so a fabricated
  value exercises exactly the same path. Make fakes obviously fake (`examplefake`,
  `EXAMPLE`, sequential `0123…`).
- **Project / app / host names** — use placeholders (`acme`, `globex`,
  `acme-web`, `acme.fly.dev`), never a real repo, Fly app, chain, or any other
  project you (or a user) actually work on.
- **Paths & usernames** — `/Users/dev/Projects/acme`, never a real home dir,
  username, or private-repo layout.

When a parser/redactor bug shows up in real production data, reproduce its
*shape* with a fabricated input — never copy the real string into a test. A
secret committed to a public package or git history can't be unpublished.

**Enforcement.** A `secret-scan` CI job and a gitleaks pre-commit hook (both read
`.gitleaks.toml`) block any real-secret-shaped string; GitHub push protection is
on as well. Fixtures pass because the config allowlists obviously-synthetic
markers by VALUE (`examplefake`, `EXAMPLE`, sequential `0123…`) — never by file
path, so a real secret in a test file still fails. Enable the local hook once
with `pipx install pre-commit && pre-commit install`.

## Releasing

Two workflows ship things, and **neither one publishes the daemon to npm** —
that line is retired (see below).

**The daemon** — `.github/workflows/release-daemon-rs.yml`. Zero-touch: merging
any commit that touches `daemon/` builds both binaries for all six targets,
bakes the prebuilt macOS tray into the mac archives, checksums (+ minisigns,
when the key is configured) everything, cuts a GitHub Release, tags
`daemon-<version>`, and commits the stamped version back to main. The version
comes from the Conventional Commits since the last `daemon-*` tag — `feat:` →
minor, `fix:`/`perf:`/`refactor:`/`revert:` → patch, `type!:` or
`BREAKING CHANGE` → major, `chore`/`docs`/`ci`/`test` → **no release**. The last
released version is read from the **tag**, never from `daemon/Cargo.toml` (which
is CI-written output, not input). `releases/latest` is what `install.sh` and the
self-updater read, so nothing else in this repo may cut a release.

**The standalone SDKs** — `.github/workflows/release-sdks.yml` publishes
`sdks/{rust,node,python}` to crates.io / PyPI / npm via OIDC Trusted Publishing
(no long-lived tokens), each only when the manifest version isn't already on the
registry. To cut one: bump the version in the manifest and merge. A brand-new
package needs a one-time Trusted Publisher set up on the registry's website
before the OIDC flow works.

**The retired npm daemon.** The TypeScript daemon (npm name `modelstat`) is
superseded by the Rust one; its tree and its auto-publisher are both deleted.
Keep it that way: anything published under that name out of this repo takes
`releases/latest` and breaks installs. `@modelstat/mcp` in `packages/mcp`
stays publishable — `npx @modelstat/mcp` is the MCP runner.

### Observing a release

```sh
gh run list --workflow=release-daemon-rs.yml --limit 3
gh run watch <run-id> --exit-status
```

The gate step prints the resolved version and whether anything is shipping.
Verify the `daemon-<version>` tag + GitHub Release exist and that
`releases/latest` points at it.

### When a release fails

```sh
gh run view <run-id> --log-failed
```

The flow is **idempotent** — re-running (push an empty commit, or re-run the
job) converges: the gate reads the tag, so a run that died before tagging
recomputes the same version, and one that died after it skips entirely. The
Homebrew tap bump no-ops when `HOMEBREW_TAP_DISPATCH_TOKEN` is absent — a
missing tap update with a green run usually means that.

## Finished worktrees are removed

A branch's worktree is finished the moment its PR is merged or closed. Remove
it — build caches, the worktree, the local branch — with the shared script
from the core repo: `../core/scripts/worktree-clean.sh --repo . --yes`
(dry-run without `--yes`; it refuses dirty trees, open PRs, `main`, and the
worktree you run it from). A merged worktree left behind keeps its `target/`
and `node_modules/`; 75 of them once filled a 1.8 TB disk.

## CRITICAL — read last (repeated from the top)

Nobody uses this service yet — there's no data or behaviour to preserve. Every change lands as the **final, canonical version**, as if the code had always been written that way:

- **Fix the root cause, in the right place** — no hacks, workarounds, or symptom patches.
- **Delete legacy/outdated code in the same change** — migrate every caller and remove the old path; prefer the clean breaking change over the cautious additive one.
- **Leave zero cruft** — no back-compat shims, aliases, deprecated paths, dead flags, `_v1`/`_v2` forks, or commented-out code.
- **Only ordering to mind** — the daemon (npm, auto-updates) and server can't cut over atomically, so change both sides and bump; that's an ordering note, not a license for a permanent shim.
