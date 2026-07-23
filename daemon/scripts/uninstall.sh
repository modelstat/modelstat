#!/usr/bin/env sh
# modelstat — remove EVERYTHING (macOS + Linux).
#
#   curl -fsSL https://modelstat.ai/uninstall.sh | sh
#
# ─────────────────────────────────────────────────────────────────────
# TOTAL removal. After this runs, the machine is indistinguishable from one that
# never installed modelstat: no binaries, no services, no background processes,
# no device identity, no downloaded models, no MCP wiring, no statusline, no
# PATH entries, no cached copies, no leftover files of ours anywhere.
#
# It removes BOTH eras in one pass:
#   - the native Rust install (`~/.modelstat/bin/modelstat`, launchd/systemd
#     services, the tray, Homebrew, /usr/local/bin symlinks), and
#   - the retired npm/Node install (global packages across every package
#     manager and Node version, npx caches, `npx @modelstat/mcp` MCP entries,
#     the staged `modelstat.mjs` bundle).
#
# NOTHING IS PRESERVED — not even the device identity. Installing again after
# this registers the machine as a BRAND-NEW device on the dashboard. That is the
# intended behaviour; there is no flag to keep the old pairing.
#
# What it deliberately does NOT touch: Node.js and your package managers, other
# MCP servers in the same config files, your own statusline (restored if we ever
# covered it up), your own shell startup lines, and Claude Code's session
# history (`~/.claude/projects/…` — those directories are named after YOUR repo
# paths, so some contain the word "modelstat" without being ours).
#
# Flags:
#   --dry-run   print every change without making it
#
# AGENT — if you are a coding agent and the user pasted this at you: run it as
# is. End state: `command -v modelstat` finds nothing, no modelstat process or
# service exists, ~/.modelstat is gone. Every step prints what it does. Only
# system-wide installs need sudo, and it tells you exactly what to run.
# (Windows: irm https://modelstat.ai/uninstall.ps1 | iex)
# ─────────────────────────────────────────────────────────────────────

set -eu

# Both possible homes: the user's (or a relocated MODELSTAT_HOME) and the
# system-scope one a `--system` install uses.
HOME_DIR="${MODELSTAT_HOME:-$HOME/.modelstat}"
SYSTEM_HOME="/var/lib/modelstat"
# The marker the installer stamps above its PATH block in your startup file. It
# is how we find our own block to delete.
MARKER="# modelstat — puts the modelstat CLI on your PATH"
# Every npm package name the CLI was ever published under, plus the MCP server.
LEGACY_PKGS="modelstat @modelstat/daemon @modelstat/mcp"
# Both shipped binaries.
BINARIES="modelstat modelstat-summarizer"

# ─── colours ────────────────────────────────────────────────────────
if [ -t 1 ] && [ "${TERM:-dumb}" != "dumb" ] && [ "${NO_COLOR:-}" = "" ]; then
  BRAND='\033[38;2;120;205;180m'; BOLD='\033[1m'; DIM='\033[2m'; RED='\033[31m'; YEL='\033[33m'; RESET='\033[0m'
else
  BRAND=''; BOLD=''; DIM=''; RED=''; YEL=''; RESET=''
fi
say()  { printf "%b\n" "$*"; }
step() { printf "\n%b▸ %b%s%b\n" "$BRAND" "$BOLD" "$1" "$RESET"; }
ok()   { printf "%b✓%b %s\n" "$BRAND" "$RESET" "$1"; REMOVED=$((REMOVED + 1)); }
skip() { printf "%b·%b %s\n" "$DIM" "$RESET" "$1"; }
warn() { printf "%b! %s%b\n" "$YEL" "$1" "$RESET"; }
die()  { printf "%b✗ %s%b\n" "$RED" "$1" "$RESET" >&2; exit 1; }

REMOVED=0
NEEDS_SUDO=""

# ─── flags ──────────────────────────────────────────────────────────
DRY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run|-n) DRY=1; shift ;;
    *) die "unknown flag: $1 (the only flag is --dry-run)" ;;
  esac
done

say ""; say "${BRAND}${BOLD}  modelstat — removing everything${RESET}"
if [ -n "$DRY" ]; then
  say "${DIM}  dry run — nothing will be changed${RESET}"
else
  say "${DIM}  this deletes your device identity and downloaded models too${RESET}"
fi
say ""

OS="$(uname -s 2>/dev/null || echo unknown)"
IS_ROOT=""; [ "$(id -u 2>/dev/null || echo 1)" = 0 ] && IS_ROOT=1

# `run` is the single choke point for every mutation: under --dry-run it prints
# the command instead of running it, so a dry run can never lie about the real one.
run() {
  if [ -n "$DRY" ]; then printf "%b    would run: %s%b\n" "$DIM" "$*" "$RESET"; return 0; fi
  "$@" >/dev/null 2>&1 || true
}
# Delete quietly; returns 1 when there was nothing there.
rm_path() {
  [ -e "$1" ] || [ -L "$1" ] || return 1
  if [ -n "$DRY" ]; then printf "%b    would delete: %s%b\n" "$DIM" "$1" "$RESET"; return 0; fi
  rm -rf "$1" 2>/dev/null || return 1
}
# Delete and report, including the "not ours to delete" case a root-owned path
# hits without sudo — collected and printed as one actionable block at the end
# rather than lost in the scroll.
sweep() {
  [ -e "$1" ] || [ -L "$1" ] || return 0
  if [ -n "$DRY" ]; then printf "%b    would delete: %s%b\n" "$DIM" "$1" "$RESET"; ok "$2"; return 0; fi
  if rm -rf "$1" 2>/dev/null; then ok "$2"
  else warn "needs elevation: $1"; NEEDS_SUDO="$NEEDS_SUDO $1"; fi
}

# ─── a JSON runtime for the config edits ────────────────────────────
# MCP entries, the statusline and the plugin registration live in JSON we must
# edit surgically — drop OUR key, keep every other server and setting the user
# has. node first (an old install means node is usually present), then python3.
# Writes are atomic (temp file + rename) and leave NO .bak litter behind.
JSONRT=""
if command -v node >/dev/null 2>&1 && node -e '' >/dev/null 2>&1; then
  JSONRT=node
elif command -v python3 >/dev/null 2>&1 && PYTHONDONTWRITEBYTECODE=1 python3 -c 'import json' >/dev/null 2>&1; then
  JSONRT=python3
fi

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

if [ "$JSONRT" = node ]; then
  cat > "$TMP/prune.js" <<'NODE_EOF'
// argv: <mode: mcp|claude|topkeys> <file>. Prints one line naming what changed.
// Exit 0 = handled, 4 = unparseable (caller reports it for manual cleanup).
const fs = require("fs");
const [mode, file] = process.argv.slice(2);
const DRY = process.env.MODELSTAT_DRY === "1";
let cfg;
try { cfg = JSON.parse(fs.readFileSync(file, "utf8")); } catch (e) {
  process.exit(e.code === "ENOENT" ? 0 : 4);
}
const changes = [];
const isOurs = (s) => /modelstat/i.test(String(s || ""));
if (mode === "mcp") {
  // One shape per tool family: mcpServers (Claude, Cursor, Windsurf, Gemini),
  // servers (VS Code), context_servers (Zed) — plus Claude Code's per-project
  // copies under projects.<dir>.mcpServers. Every modelstat entry goes now,
  // npx-era and native alike.
  const buckets = [];
  for (const k of ["mcpServers", "servers", "context_servers"]) {
    if (cfg && typeof cfg[k] === "object" && cfg[k]) buckets.push([k, cfg[k]]);
  }
  if (cfg && typeof cfg.projects === "object" && cfg.projects) {
    for (const [dir, p] of Object.entries(cfg.projects)) {
      if (p && typeof p.mcpServers === "object" && p.mcpServers) buckets.push([`projects[${dir}]`, p.mcpServers]);
    }
  }
  for (const [label, bucket] of buckets) {
    for (const name of Object.keys(bucket)) {
      if (isOurs(name)) { delete bucket[name]; changes.push(`${label}.${name}`); }
    }
  }
} else if (mode === "claude") {
  // Statusline: ours goes whatever form it is in. If we had composed over a
  // statusline of the user's, that one is stashed here and comes back.
  const sl = cfg.statusLine;
  const cmd = sl && typeof sl.command === "string" ? sl.command : "";
  if (isOurs(cmd)) {
    const prev = cfg._modelstatPrevStatusLine;
    if (prev) { cfg.statusLine = prev; changes.push("statusLine (restored yours)"); }
    else { delete cfg.statusLine; changes.push("statusLine"); }
  }
  if (Object.prototype.hasOwnProperty.call(cfg, "_modelstatPrevStatusLine")) {
    delete cfg._modelstatPrevStatusLine; changes.push("_modelstatPrevStatusLine");
  }
  // Claude Code plugin registration (the /stat command + bundled MCP server).
  for (const k of ["extraKnownMarketplaces", "enabledPlugins", "disabledPlugins"]) {
    if (cfg[k] && typeof cfg[k] === "object") {
      for (const name of Object.keys(cfg[k])) {
        if (isOurs(name)) { delete cfg[k][name]; changes.push(`${k}.${name}`); }
      }
      if (Object.keys(cfg[k]).length === 0) delete cfg[k];
    }
  }
} else if (mode === "topkeys") {
  // known_marketplaces.json — a flat map keyed by marketplace name.
  for (const name of Object.keys(cfg)) {
    if (isOurs(name)) { delete cfg[name]; changes.push(name); }
  }
}
if (!changes.length) process.exit(0);
if (!DRY) {
  const tmp = `${file}.modelstat-tmp-${process.pid}`;
  fs.writeFileSync(tmp, `${JSON.stringify(cfg, null, 2)}\n`);
  fs.renameSync(tmp, file);           // atomic; no .bak left behind
}
console.log(changes.join(", "));
NODE_EOF
elif [ "$JSONRT" = python3 ]; then
  cat > "$TMP/prune.py" <<'PY_EOF'
# argv: <mode: mcp|claude|topkeys> <file>. Prints one line naming what changed.
# Exit 0 = handled, 4 = unparseable (caller reports it for manual cleanup).
import json, os, re, sys

mode, file = sys.argv[1], sys.argv[2]
DRY = os.environ.get("MODELSTAT_DRY") == "1"
try:
    with open(file, encoding="utf-8") as fh:
        cfg = json.load(fh)
except FileNotFoundError:
    sys.exit(0)
except Exception:
    sys.exit(4)

changes = []
def is_ours(s):
    return bool(re.search("modelstat", str(s or ""), re.I))

if mode == "mcp":
    # One shape per tool family: mcpServers (Claude, Cursor, Windsurf, Gemini),
    # servers (VS Code), context_servers (Zed) — plus Claude Code's per-project
    # copies. Every modelstat entry goes now, npx-era and native alike.
    buckets = [(k, cfg[k]) for k in ("mcpServers", "servers", "context_servers")
               if isinstance(cfg.get(k), dict)]
    if isinstance(cfg.get("projects"), dict):
        for d, p in cfg["projects"].items():
            if isinstance(p, dict) and isinstance(p.get("mcpServers"), dict):
                buckets.append((f"projects[{d}]", p["mcpServers"]))
    for label, bucket in buckets:
        for name in [n for n in bucket if is_ours(n)]:
            del bucket[name]
            changes.append(f"{label}.{name}")
elif mode == "claude":
    # Statusline: ours goes whatever form it is in. A statusline of the user's
    # that we had composed over is stashed here and comes back.
    sl = cfg.get("statusLine")
    cmd = sl.get("command") if isinstance(sl, dict) else ""
    if is_ours(cmd):
        prev = cfg.get("_modelstatPrevStatusLine")
        if prev:
            cfg["statusLine"] = prev
            changes.append("statusLine (restored yours)")
        else:
            cfg.pop("statusLine", None)
            changes.append("statusLine")
    if "_modelstatPrevStatusLine" in cfg:
        del cfg["_modelstatPrevStatusLine"]
        changes.append("_modelstatPrevStatusLine")
    # Claude Code plugin registration (the /stat command + bundled MCP server).
    for k in ("extraKnownMarketplaces", "enabledPlugins", "disabledPlugins"):
        if isinstance(cfg.get(k), dict):
            for name in [n for n in cfg[k] if is_ours(n)]:
                del cfg[k][name]
                changes.append(f"{k}.{name}")
            if not cfg[k]:
                del cfg[k]
elif mode == "topkeys":
    # known_marketplaces.json — a flat map keyed by marketplace name.
    for name in [n for n in cfg if is_ours(n)]:
        del cfg[name]
        changes.append(name)

if not changes:
    sys.exit(0)
if not DRY:
    tmp = f"{file}.modelstat-tmp-{os.getpid()}"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(cfg, fh, indent=2, ensure_ascii=False)
        fh.write("\n")
    os.replace(tmp, file)             # atomic; no .bak left behind
print(", ".join(changes))
PY_EOF
fi

UNPARSEABLE=""
json_prune() {
  mode="$1"; f="$2"
  [ -f "$f" ] || return 0
  if [ -z "$JSONRT" ]; then
    grep -qi 'modelstat' "$f" 2>/dev/null && UNPARSEABLE="$UNPARSEABLE
    $f"
    return 0
  fi
  set +e
  if [ "$JSONRT" = node ]; then
    out="$(MODELSTAT_DRY="${DRY:-0}" node "$TMP/prune.js" "$mode" "$f" 2>/dev/null)"; rc=$?
  else
    # PYTHONDONTWRITEBYTECODE: keep python from dropping a __pycache__ /
    # ~/Library/Caches/com.apple.python trail — an uninstaller must not leave
    # files behind, least of all during --dry-run.
    out="$(MODELSTAT_DRY="${DRY:-0}" PYTHONDONTWRITEBYTECODE=1 python3 "$TMP/prune.py" "$mode" "$f" 2>/dev/null)"; rc=$?
  fi
  set -e
  if [ "$rc" = 4 ]; then
    # JSONC with comments (VS Code, Zed) or a hand-broken file — never guess.
    grep -qi 'modelstat' "$f" 2>/dev/null && UNPARSEABLE="$UNPARSEABLE
    $f"
    return 0
  fi
  [ -n "$out" ] && ok "$f — removed $out"
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# 1. Services — every component, both scopes
# ─────────────────────────────────────────────────────────────────────
# Three launchd agents (collector, summariser engine, menu-bar tray) and two
# systemd units. Boot them out BEFORE deleting anything they run, or the
# supervisor restarts the process while we are mid-delete.
step "Background services"
SVC_HIT=""
if [ "$OS" = Darwin ]; then
  for label in ai.modelstat.daemon ai.modelstat.summarizer ai.modelstat.tray; do
    plist="$HOME/Library/LaunchAgents/$label.plist"
    if [ -f "$plist" ]; then
      run launchctl bootout "gui/$(id -u)/$label"
      rm_path "$plist" && ok "removed launchd agent $label"
      SVC_HIT=1
    fi
    # System scope (a `--system` install) lives in LaunchDaemons and needs root.
    sysplist="/Library/LaunchDaemons/$label.plist"
    if [ -f "$sysplist" ]; then
      SVC_HIT=1
      if [ -n "$IS_ROOT" ]; then
        run launchctl bootout "system/$label"
        sweep "$sysplist" "removed system LaunchDaemon $label"
      else
        warn "needs elevation: $sysplist"; NEEDS_SUDO="$NEEDS_SUDO $sysplist"
      fi
    fi
  done
elif [ "$OS" = Linux ]; then
  for unit in modelstat modelstat-summarizer; do
    upath="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$unit.service"
    if [ -f "$upath" ]; then
      run systemctl --user disable --now "$unit.service"
      rm_path "$upath" && ok "removed systemd unit $unit.service"
      SVC_HIT=1
    fi
    syspath="/etc/systemd/system/$unit.service"
    if [ -f "$syspath" ]; then
      SVC_HIT=1
      if [ -n "$IS_ROOT" ]; then
        run systemctl disable --now "$unit.service"
        sweep "$syspath" "removed system unit $unit.service"
      else
        warn "needs elevation: $syspath"; NEEDS_SUDO="$NEEDS_SUDO $syspath"
      fi
    fi
  done
  run systemctl --user daemon-reload
  [ -n "$IS_ROOT" ] && run systemctl daemon-reload
fi
[ -n "$SVC_HIT" ] || skip "no services installed"

# ─────────────────────────────────────────────────────────────────────
# 2. Any surviving process
# ─────────────────────────────────────────────────────────────────────
# Hand-started daemons, a tray whose agent we just deleted, or anything the
# supervisor did not reap. Patterns are anchored on our own binary and bundle
# names so nothing unrelated matches.
step "Running processes"
PROC_HIT=""
if command -v pgrep >/dev/null 2>&1; then
  for pat in 'modelstat\.mjs' '\.modelstat/bin/modelstat' 'modelstat-summarizer' 'ModelstatTray'; do
    if pgrep -f "$pat" >/dev/null 2>&1; then
      run pkill -TERM -f "$pat"
      ok "stopped processes matching $pat"
      PROC_HIT=1
    fi
  done
fi
[ -n "$PROC_HIT" ] || skip "nothing running"

# ─────────────────────────────────────────────────────────────────────
# 3. The menu-bar tray app (macOS)
# ─────────────────────────────────────────────────────────────────────
step "Menu-bar tray app"
if [ "$OS" = Darwin ] && [ -d "$HOME/Applications/ModelstatTray.app" ]; then
  sweep "$HOME/Applications/ModelstatTray.app" "removed ~/Applications/ModelstatTray.app"
else
  skip "not installed"
fi

# ─────────────────────────────────────────────────────────────────────
# 4. Homebrew
# ─────────────────────────────────────────────────────────────────────
# The native install also ships as a tap formula. Left alone, `brew upgrade`
# would cheerfully reinstall everything we are deleting.
step "Homebrew"
if command -v brew >/dev/null 2>&1 && brew list --formula 2>/dev/null | grep -qx 'modelstat'; then
  run brew uninstall --force modelstat
  ok "brew: uninstalled modelstat"
  if brew tap 2>/dev/null | grep -qx 'modelstat/tap'; then
    run brew untap modelstat/tap
    ok "brew: untapped modelstat/tap"
  fi
else
  skip "not installed via Homebrew"
fi

# ─────────────────────────────────────────────────────────────────────
# 5. Global npm-era packages — ask, then sweep
# ─────────────────────────────────────────────────────────────────────
# Asking a package manager is NOT enough on its own: pnpm resolves `-g` against
# the CURRENT store version, so a package installed by an older pnpm
# (…/pnpm/global/5) is invisible to a newer one (…/global/v11) — it reports "no
# global packages" and removes nothing while the command still works. nvm/fnm/
# volta have the same shape, one global root per Node version. So we ask first
# (it keeps their metadata consistent) and then sweep every root on disk.
step "Global packages — pass A: ask each package manager"
PKG_HIT=""
for pkg in $LEGACY_PKGS; do
  if command -v npm >/dev/null 2>&1 && npm ls -g --depth=0 "$pkg" >/dev/null 2>&1; then
    run npm rm -g "$pkg"; ok "npm: removed $pkg"; PKG_HIT=1
  fi
  if command -v pnpm >/dev/null 2>&1 && pnpm ls -g --depth 0 2>/dev/null | grep -q "^$pkg \|^$pkg@\| $pkg "; then
    run pnpm remove -g "$pkg"; ok "pnpm: removed $pkg"; PKG_HIT=1
  fi
  # yarn v1 is the only yarn with a global registry; berry exits nonzero here.
  if command -v yarn >/dev/null 2>&1 && yarn global list 2>/dev/null | grep -q "$pkg@"; then
    run yarn global remove "$pkg"; ok "yarn: removed $pkg"; PKG_HIT=1
  fi
  if command -v bun >/dev/null 2>&1 && bun pm ls -g 2>/dev/null | grep -q "$pkg@"; then
    run bun remove -g "$pkg"; ok "bun: removed $pkg"; PKG_HIT=1
  fi
done
if command -v volta >/dev/null 2>&1 && volta list 2>/dev/null | grep -q 'modelstat'; then
  run volta uninstall modelstat; ok "volta: removed modelstat"; PKG_HIT=1
fi
[ -n "$PKG_HIT" ] || skip "no manager reported a global install"

step "Global packages — pass B: sweep every global root on disk"
# Globs are deliberately unquoted so `*` expands across store versions and Node
# versions; a glob matching nothing stays literal and fails the -d test below.
DISK_HIT=""
for nm in \
  "$(npm root -g 2>/dev/null || true)" \
  "$HOME"/.nvm/versions/node/*/lib/node_modules \
  "$HOME"/.local/share/fnm/node-versions/*/installation/lib/node_modules \
  "$HOME"/Library/Application\ Support/fnm/node-versions/*/installation/lib/node_modules \
  "$HOME"/.volta/tools/image/packages \
  "$HOME"/Library/pnpm/global/*/node_modules \
  "${XDG_DATA_HOME:-$HOME/.local/share}"/pnpm/global/*/node_modules \
  ${PNPM_HOME:+"$PNPM_HOME"/global/*/node_modules} \
  "$HOME"/.bun/install/global/node_modules \
  "$HOME"/.config/yarn/global/node_modules \
  "$HOME"/.yarn/global/node_modules \
  /usr/local/lib/node_modules \
  /opt/homebrew/lib/node_modules \
  /opt/homebrew/opt/node@*/lib/node_modules
do
  [ -d "$nm" ] || continue
  for pkg in modelstat @modelstat; do
    if [ -e "$nm/$pkg" ]; then sweep "$nm/$pkg" "removed $nm/$pkg"; DISK_HIT=1; fi
  done
done
# pnpm keeps the real package files in a content-addressed store beside
# node_modules; removing only the link would strand hundreds of megabytes.
for st in "$HOME"/Library/pnpm/global/*/.pnpm \
          "${XDG_DATA_HOME:-$HOME/.local/share}"/pnpm/global/*/.pnpm \
          ${PNPM_HOME:+"$PNPM_HOME"/global/*/.pnpm}; do
  [ -d "$st" ] || continue
  for d in "$st"/modelstat@* "$st"/@modelstat*; do
    if [ -e "$d" ]; then sweep "$d" "removed $d"; DISK_HIT=1; fi
  done
done
[ -n "$DISK_HIT" ] || skip "no leftover package directories"

# ─────────────────────────────────────────────────────────────────────
# 6. npx / dlx caches
# ─────────────────────────────────────────────────────────────────────
# `npx modelstat@latest` never installed globally — it cached a full copy that
# `npx modelstat` would keep re-running (and reinstalling the service from).
step "npx / dlx caches"
CACHE_HIT=""
for root in "$HOME/.npm/_npx" "$HOME/Library/Caches/pnpm/dlx" "${XDG_CACHE_HOME:-$HOME/.cache}/pnpm/dlx"; do
  [ -d "$root" ] || continue
  for d in "$root"/*/; do
    [ -d "$d" ] || continue
    if [ -e "$d/node_modules/modelstat" ] || [ -e "$d/node_modules/@modelstat" ]; then
      rm_path "$d" && CACHE_HIT=1
    fi
  done
done
for d in "$HOME/.bun/install/cache/modelstat"* "$HOME/.bun/install/cache/@modelstat"*; do
  [ -e "$d" ] && rm_path "$d" && CACHE_HIT=1
done
if [ -n "$CACHE_HIT" ]; then ok "cleared cached copies"; else skip "no cached copies"; fi

# ─────────────────────────────────────────────────────────────────────
# 7. Command launchers on disk
# ─────────────────────────────────────────────────────────────────────
# Both binary names, everywhere a package manager or the installer could have
# put them: the current PATH, every package-manager bin dir (a shim under a Node
# version that is not active right now is invisible to PATH but returns the
# moment the user switches versions), and the `--system` symlinks in
# /usr/local/bin. Everything named `modelstat` goes — there is no version of
# this tool we are keeping.
step "Command launchers"
SHIM_HIT=""
BIN_LIST="$(
  printf '%s\n' "$PATH" | tr ':' '\n'
  printf '%s\n' \
    "$HOME/.bun/bin" "$HOME/.config/yarn/bin" "$HOME/.yarn/bin" "$HOME/.volta/bin" \
    "$HOME/Library/pnpm" "${XDG_DATA_HOME:-$HOME/.local/share}/pnpm" \
    "$HOME_DIR/bin" /usr/local/bin /opt/homebrew/bin
  [ -n "${PNPM_HOME:-}" ] && printf '%s\n' "$PNPM_HOME"
  npm prefix -g 2>/dev/null | sed 's|$|/bin|'
  ls -d "$HOME"/.nvm/versions/node/*/bin 2>/dev/null || true
  ls -d "$HOME"/.local/share/fnm/node-versions/*/installation/bin 2>/dev/null || true
  ls -d /opt/homebrew/opt/node@*/bin 2>/dev/null || true
)"
BIN_LIST="$(printf '%s\n' "$BIN_LIST" | awk 'NF && !seen[$0]++')"
# `while … done <<EOF` keeps the loop in THIS shell (a pipeline would fork a
# subshell and lose every counter it sets).
while IFS= read -r d; do
  [ -n "$d" ] && [ -d "$d" ] || continue
  for b in $BINARIES; do
    f="$d/$b"
    [ -e "$f" ] || [ -L "$f" ] || continue
    sweep "$f" "removed $f"
    SHIM_HIT=1
  done
done <<EOF
$BIN_LIST
EOF
[ -n "$SHIM_HIT" ] || skip "no launchers found"

# ─────────────────────────────────────────────────────────────────────
# 8. MCP wiring in every AI tool
# ─────────────────────────────────────────────────────────────────────
# Both eras: the `npx @modelstat/mcp` entry and the native absolute-path one.
# Other servers in the same files are untouched.
step "MCP entries in your AI tools"
SUP="$HOME/Library/Application Support"
for f in \
  "$HOME/.claude.json" \
  "$SUP/Claude/claude_desktop_config.json" \
  "$HOME/.config/Claude/claude_desktop_config.json" \
  "$HOME/.cursor/mcp.json" \
  "$SUP/Code/User/mcp.json" \
  "$HOME/.config/Code/User/mcp.json" \
  "$SUP/Code - Insiders/User/mcp.json" \
  "$HOME/.config/Code - Insiders/User/mcp.json" \
  "$SUP/VSCodium/User/mcp.json" \
  "$HOME/.config/VSCodium/User/mcp.json" \
  "$HOME/.codeium/windsurf/mcp_config.json" \
  "$HOME/.gemini/settings.json" \
  "$HOME/.config/zed/settings.json"
do
  json_prune mcp "$f"
done

# Claude Code owns a user-scope registry through its CLI — ask it first so its
# own bookkeeping stays consistent; the ~/.claude.json pass above catches the rest.
if command -v claude >/dev/null 2>&1 && claude mcp list 2>/dev/null | grep -q 'modelstat'; then
  run claude mcp remove modelstat -s user
  ok "claude: removed the modelstat MCP entry"
fi

# Codex stores MCP servers as a TOML table, so it needs its own pass: keep the
# file byte-for-byte except our block.
CODEX="$HOME/.codex/config.toml"
if [ -f "$CODEX" ] && grep -q '^\[mcp_servers\.modelstat\]' "$CODEX"; then
  if [ -n "$DRY" ]; then
    say "${DIM}    would edit: $CODEX (drop [mcp_servers.modelstat])${RESET}"
  else
    awk '/^\[mcp_servers\.modelstat\]/{skip=1;next} /^\[/{skip=0} !skip' "$CODEX" > "$CODEX.modelstat-tmp"
    mv "$CODEX.modelstat-tmp" "$CODEX"
  fi
  ok "$CODEX — removed [mcp_servers.modelstat]"
fi

# ─────────────────────────────────────────────────────────────────────
# 9. Claude Code statusline + plugin
# ─────────────────────────────────────────────────────────────────────
# NOTE: this touches ~/.claude/settings.json and the plugin registry ONLY. It
# never goes near ~/.claude/projects/ — those directories are named after your
# repo paths, so some contain "modelstat" while being your own session history.
step "Claude Code statusline + plugin"
CLAUDE_DIRS="$(printf '%s\n%s\n' "${CLAUDE_CONFIG_DIR:-$HOME/.claude}" "$HOME/.claude" | awk 'NF && !seen[$0]++')"
while IFS= read -r cdir; do
  [ -n "$cdir" ] || continue
  json_prune claude "$cdir/settings.json"
  json_prune topkeys "$cdir/plugins/known_marketplaces.json"
  for d in "$cdir"/plugins/marketplaces/*modelstat*; do
    [ -e "$d" ] && sweep "$d" "removed plugin marketplace $d"
  done
  # Backups our own installers left behind on earlier runs.
  for b in "$cdir"/settings.json.modelstat.bak "$cdir"/settings.json.modelstat-legacy.bak; do
    rm_path "$b" && ok "removed leftover backup $b"
  done
done <<EOF
$CLAUDE_DIRS
EOF

# ─────────────────────────────────────────────────────────────────────
# 10. PATH entries in shell startup files
# ─────────────────────────────────────────────────────────────────────
# Our block is the marker line plus the `.` line under it. Left behind, that
# line errors on every new shell once the home dir is gone. Any other line that
# puts a modelstat directory on PATH goes too. Lines merely MENTIONING modelstat
# are reported, never silently deleted — they might be yours.
step "PATH entries in shell startup files"
RC_HIT=""
for rc in "$HOME/.zshrc" "$HOME/.zprofile" "$HOME/.zshenv" "$HOME/.bashrc" \
          "$HOME/.bash_profile" "$HOME/.profile" "$HOME/.config/fish/config.fish"; do
  [ -f "$rc" ] || continue
  grep -q 'modelstat' "$rc" || continue
  MATCHES="$(awk -v m="$MARKER" '
    $0 == m { print NR": "$0; nxt = 1; next }
    nxt == 1 { print NR": "$0; nxt = 0; next }
    /modelstat/ && /PATH/ { print NR": "$0 }
  ' "$rc")"
  if [ -n "$MATCHES" ]; then
    RC_HIT=1
    say "$MATCHES" | sed 's/^/    /'
    if [ -n "$DRY" ]; then
      say "${DIM}    would edit: $rc${RESET}"
    else
      awk -v m="$MARKER" '
        $0 == m { nxt = 1; next }
        nxt == 1 { nxt = 0; next }
        /modelstat/ && /PATH/ { next }
        { print }
      ' "$rc" > "$rc.modelstat-tmp"
      mv "$rc.modelstat-tmp" "$rc"
    fi
    ok "$rc — removed our PATH block"
  fi
  LEFT="$(grep -n 'modelstat' "$rc" 2>/dev/null || true)"
  if [ -n "$LEFT" ]; then
    warn "$rc still mentions modelstat (left alone — yours to review):"
    say "$LEFT" | sed 's/^/    /'
  fi
done
# fish gets its own drop-in file, which fish auto-sources — delete it outright.
rm_path "$HOME/.config/fish/conf.d/modelstat.fish" && { ok "removed the fish drop-in"; RC_HIT=1; }
[ -n "$RC_HIT" ] || skip "no PATH entries"

# ─────────────────────────────────────────────────────────────────────
# 11. The home directories — identity, models, logs, binaries, everything
# ─────────────────────────────────────────────────────────────────────
step "Data directories"
HOME_HIT=""
# MODELSTAT_HOME may point elsewhere, so both it and the default are checked —
# deduped, because they are usually the same path.
HOME_SEEN=""
for d in "$HOME_DIR" "$HOME/.modelstat" "$SYSTEM_HOME"; do
  [ -e "$d" ] || continue
  case " $HOME_SEEN " in *" $d "*) continue ;; esac
  HOME_SEEN="$HOME_SEEN $d"
  if [ "$d" = "$SYSTEM_HOME" ] && [ -z "$IS_ROOT" ]; then
    warn "needs elevation: $d"; NEEDS_SUDO="$NEEDS_SUDO $d"; HOME_HIT=1; continue
  fi
  sweep "$d" "deleted $d (identity, models, logs, binaries)"
  HOME_HIT=1
done
[ -n "$HOME_HIT" ] || skip "nothing to delete"

# ─── summary ────────────────────────────────────────────────────────
say ""
if [ -n "$UNPARSEABLE" ]; then
  warn "these files mention modelstat but couldn't be parsed automatically —"
  warn "remove the \"modelstat\" entry by hand (they're likely JSON with comments):"
  say "$UNPARSEABLE"
  say ""
fi
if [ -z "$JSONRT" ]; then
  warn "no node or python3 found — MCP, statusline and plugin entries were only reported, not removed."
  say ""
fi
if [ -n "$NEEDS_SUDO" ]; then
  warn "a system-wide install needs root. Finish with:"
  say "    sudo rm -rf$NEEDS_SUDO"
  say ""
fi

if [ "$REMOVED" = 0 ]; then
  say "${BRAND}${BOLD}  Nothing found — this machine is already clean.${RESET}"
else
  say "${BRAND}${BOLD}  Done — modelstat is gone ($REMOVED item(s) removed).${RESET}"
fi
say ""
say "${DIM}  Open a NEW terminal, then confirm:${RESET}"
say "${DIM}    command -v modelstat     # expect no output${RESET}"
say "${DIM}    ls ~/.modelstat          # expect: No such file or directory${RESET}"
say ""
say "${DIM}  Your account and history on modelstat.ai are server-side and untouched —${RESET}"
say "${DIM}  delete those from the dashboard if you want them gone too.${RESET}"
say ""
