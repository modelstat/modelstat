//! Source Discovery Engine — a port of `packages/parsers/src/discovery/index.ts`.
//!
//! Runs every strategy and returns a merged list of detected installations +
//! identities. Deterministic, idempotent — safe on a cron (feature §7.2). The
//! macOS `system_profiler` app-registry strategy is DROPPED (feature §22 — it
//! never populated bundle ids, so `bundleIds` matching was dead). Windows
//! data-dir + binary-dir equivalents are ADDED (§7.2).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::util::run_command;

/// One detected tool install on a device (feature §17 wire shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedInstallation {
    pub agent: String,
    pub install_method: String,
    pub binary_path: Option<String>,
    pub data_dir: Option<String>,
    pub version: Option<String>,
    pub detected_via: Vec<String>,
}

/// A source-account identity detected on the device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedIdentity {
    pub provider: String,
    pub provider_account_id: String,
    pub provider_account_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_org: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub owner_scope: String,
    pub detection_source: String,
}

/// The output of a discovery pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryOutput {
    pub installations: Vec<DetectedInstallation>,
    pub identities: Vec<DetectedIdentity>,
}

struct SourceSpec {
    agent: &'static str,
    macos: &'static [&'static str],
    linux: &'static [&'static str],
    windows: &'static [&'static str],
    data_dir_env: &'static [&'static str],
    binaries: &'static [&'static str],
}

/// Registry of sources. Extend this list to add a new agent — no code changes in
/// the discovery logic itself. `bundleIds` are dropped with the app-registry
/// strategy (§22).
fn sources() -> &'static [SourceSpec] {
    &[
        SourceSpec {
            agent: "claude_code",
            macos: &["~/.claude"],
            linux: &["$XDG_CONFIG_HOME/claude", "~/.claude", "~/.config/claude"],
            windows: &["~/.claude"],
            data_dir_env: &["CLAUDE_HOME"],
            binaries: &["claude", "claude-code"],
        },
        SourceSpec {
            agent: "codex_cli",
            macos: &["~/.codex"],
            linux: &["$XDG_CONFIG_HOME/codex", "~/.codex"],
            windows: &["~/.codex"],
            data_dir_env: &["CODEX_HOME"],
            binaries: &["codex"],
        },
        SourceSpec {
            agent: "claude_desktop",
            macos: &["~/Library/Application Support/Claude"],
            linux: &["~/.config/Claude"],
            windows: &["$APPDATA/Claude"],
            data_dir_env: &[],
            binaries: &[],
        },
        SourceSpec {
            agent: "cursor",
            macos: &["~/Library/Application Support/Cursor"],
            linux: &["~/.config/Cursor"],
            windows: &["$APPDATA/Cursor"],
            data_dir_env: &[],
            binaries: &[],
        },
        SourceSpec {
            agent: "windsurf",
            macos: &["~/Library/Application Support/Windsurf"],
            linux: &["~/.config/Windsurf"],
            windows: &["$APPDATA/Windsurf"],
            data_dir_env: &[],
            binaries: &[],
        },
        SourceSpec {
            agent: "zed",
            macos: &["~/Library/Application Support/Zed", "~/.config/zed"],
            linux: &["~/.config/zed"],
            windows: &["$APPDATA/Zed"],
            data_dir_env: &[],
            binaries: &["zed"],
        },
        SourceSpec {
            agent: "gemini_cli",
            macos: &["~/.gemini"],
            linux: &["~/.gemini"],
            windows: &["~/.gemini"],
            data_dir_env: &[],
            binaries: &["gemini"],
        },
        SourceSpec {
            agent: "aider",
            macos: &["~/.aider"],
            linux: &["~/.aider"],
            windows: &["~/.aider"],
            data_dir_env: &[],
            binaries: &["aider"],
        },
        SourceSpec {
            agent: "ollama",
            macos: &["~/.ollama"],
            linux: &["~/.ollama"],
            windows: &["~/.ollama"],
            data_dir_env: &[],
            binaries: &["ollama"],
        },
        SourceSpec {
            agent: "pi",
            macos: &["~/.pi/agent", "~/.omp/agent"],
            linux: &[
                "$XDG_CONFIG_HOME/pi/agent",
                "~/.pi/agent",
                "$XDG_CONFIG_HOME/omp/agent",
                "~/.omp/agent",
            ],
            windows: &["~/.pi/agent", "~/.omp/agent"],
            data_dir_env: &["PI_HOME", "OMP_HOME"],
            binaries: &["pi", "omp"],
        },
        SourceSpec {
            agent: "openclaw",
            macos: &["~/.openclaw", "~/.claw"],
            linux: &["~/.openclaw", "~/.claw"],
            windows: &["~/.openclaw", "~/.claw"],
            data_dir_env: &[],
            binaries: &["openclaw", "claw", "clawdbot", "moltbot"],
        },
    ]
}

/// Which strategies to skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    BinaryWalk,
    FileSignatures,
    ProcessProbe,
}

/// Options for a discovery pass.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryOptions {
    pub extra_data_dirs: HashMap<String, Vec<String>>,
    pub skip: Vec<Strategy>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Os {
    Macos,
    Linux,
    Windows,
}

fn current_os() -> Os {
    match std::env::consts::OS {
        "macos" => Os::Macos,
        "windows" => Os::Windows,
        _ => Os::Linux,
    }
}

/// Run every discovery strategy and return the merged, deduped result.
pub fn discover(options: &DiscoveryOptions) -> DiscoveryOutput {
    let os = current_os();
    let skip_binary_walk = options.skip.contains(&Strategy::BinaryWalk);

    let mut installations: Vec<DetectedInstallation> = Vec::new();
    let mut identities: Vec<DetectedIdentity> = Vec::new();

    // (1) known-path probe — an existing data dir per source.
    for spec in sources() {
        let mut candidates: Vec<String> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let add = |c: String, candidates: &mut Vec<String>, seen: &mut BTreeSet<String>| {
            if seen.insert(c.clone()) {
                candidates.push(c);
            }
        };
        for raw in os_dirs(spec, os) {
            add(expand_path(raw), &mut candidates, &mut seen);
        }
        for env in spec.data_dir_env {
            if let Ok(v) = std::env::var(env) {
                if !v.is_empty() {
                    add(v, &mut candidates, &mut seen);
                }
            }
        }
        if let Some(extra) = options.extra_data_dirs.get(spec.agent) {
            for e in extra {
                add(expand_path(e), &mut candidates, &mut seen);
            }
        }
        for p in candidates {
            if Path::new(&p).is_dir() {
                installations.push(DetectedInstallation {
                    agent: spec.agent.to_string(),
                    install_method: "manual".to_string(),
                    binary_path: None,
                    data_dir: Some(p),
                    version: None,
                    detected_via: vec!["known_path".to_string()],
                });
            }
        }
    }

    // (2) process probe — what is RUNNING right now. The path strategies only
    // find an agent where we thought to look; this one finds it wherever it
    // actually lives, which is the whole point for a tool installed somewhere
    // nobody enumerated.
    if !options.skip.contains(&Strategy::ProcessProbe) {
        installations.extend(probe_processes(os));
    }

    // (3) binary walk.
    if !skip_binary_walk {
        let bin_dirs = binary_lookup_dirs(os);
        for spec in sources() {
            for bin in spec.binaries {
                for dir in &bin_dirs {
                    let p = Path::new(dir).join(bin);
                    if p.exists() {
                        let ps = p.to_string_lossy().into_owned();
                        let version = safe_version_probe(&ps);
                        installations.push(DetectedInstallation {
                            agent: spec.agent.to_string(),
                            install_method: classify_install_method(&ps, os),
                            binary_path: Some(ps),
                            data_dir: None,
                            version,
                            detected_via: vec!["binary_walk".to_string()],
                        });
                    }
                }
            }
        }
    }

    // (6) application registry (macOS `system_profiler`) — DROPPED (§22).

    // Identity probes — best-effort, filesystem + keychain.
    identities.extend(probe_identities(os));

    DiscoveryOutput {
        installations: dedupe_installs(installations),
        identities: dedupe_identities(identities),
    }
}

/// One running process, reduced to what discovery may look at.
///
/// PRIVACY: `args` exists for the lifetime of this probe and NEVER leaves it.
/// A command line routinely carries secrets — `--api-key sk-…`, a token, a
/// database URL — so nothing derived from it reaches the wire except a
/// directory path this module recognised and validated as a real directory.
struct RunningProcess {
    exe: String,
    args: Vec<String>,
}

/// Interpreters that run an agent from a script rather than a binary. Only
/// these are trusted to name their agent in argument one.
const SCRIPT_RUNNERS: &[&str] = &[
    "node", "bun", "deno", "python", "python3", "npx", "pnpm", "uv",
];

/// Command-line flags that relocate an agent's data directory. A tool started
/// with one of these keeps its sessions somewhere no path list would guess.
const DATA_DIR_FLAGS: &[&str] = &[
    "--config-dir",
    "--config-path",
    "--data-dir",
    "--home",
    "--home-dir",
    "--session-dir",
    "--sessions-dir",
    "--state-dir",
    "--user-data-dir",
];

/// Enumerate this user's running processes. Best-effort: an unavailable or
/// slow lister yields nothing rather than blocking a scan.
fn running_processes(os: Os) -> Vec<RunningProcess> {
    let out = match os {
        // `args=` only, NOT `comm=`: macOS truncates `comm` to 16 characters
        // (MAXCOMLEN), which turned every path into a stub like
        // `/Applications/Cl`. The full command line carries the untruncated
        // executable as its first token.
        Os::Macos | Os::Linux => {
            run_command("ps", &["-axo", "args="], None, Duration::from_secs(5))
        }
        // `wmic` is gone on current Windows; CIM is the supported route.
        Os::Windows => run_command(
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                "Get-CimInstance Win32_Process | ForEach-Object { \"$($_.ExecutablePath) $($_.CommandLine)\" }",
            ],
            None,
            Duration::from_secs(15),
        ),
    };
    let Some(out) = out else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            // First token is the executable, the rest its arguments. A path
            // containing spaces loses its tail here; that costs nothing,
            // because matching is by file name and the interesting arguments
            // (a relocated data dir) are later tokens either way.
            let mut tokens = line.split_whitespace().map(str::to_string);
            let exe = tokens.next()?;
            Some(RunningProcess {
                exe,
                args: tokens.collect(),
            })
        })
        .collect()
}

/// The file name of a path, lowercased, with a Windows `.exe` suffix removed —
/// what a process's executable is matched by.
fn exe_stem(path: &str) -> String {
    // Split on BOTH separators rather than deferring to `Path`, which only
    // understands the host's: a Windows command line read anywhere else keeps
    // its whole `C:\\...\\` prefix as the "file name" and matches nothing.
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name).to_string()
}

/// A data directory named by the process's own command line, e.g.
/// `--config-dir /somewhere/else`. Both `--flag value` and `--flag=value` are
/// read. Only an existing directory is returned: an unrecognised flag value is
/// dropped rather than guessed at, which also means no free-text argument can
/// slip through as a "path".
fn data_dir_from_args(args: &[String]) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (a.as_str(), None),
        };
        if !DATA_DIR_FLAGS.contains(&flag) {
            continue;
        }
        let value = inline.or_else(|| args.get(i + 1).cloned())?;
        let expanded = expand_path(&value);
        if Path::new(&expanded).is_dir() {
            return Some(expanded);
        }
    }
    None
}

/// The application-support directory belonging to a macOS bundle, derived from
/// the running executable's own path — `/Applications/Foo Bar.app/Contents/…`
/// → `~/Library/Application Support/Foo Bar`. This is how a SECOND install
/// under an unknown name ("Claude Second.app") is found: by following the
/// binary that is actually running, never by guessing the app's name.
fn bundle_data_dir(exe: &str) -> Option<String> {
    let bundle = exe.split(".app/").next()?;
    let name = Path::new(bundle)
        .file_name()?
        .to_string_lossy()
        .into_owned();
    if name.is_empty() || !exe.contains(".app/") {
        return None;
    }
    let parent = expand_path("~/Library/Application Support");
    // Case-SENSITIVE match on purpose. macOS filesystems are usually
    // case-insensitive, so a bundle named `claude.app` living inside a second
    // instance's tree would happily resolve to the FIRST install's `Claude`
    // directory and report one install's sessions as another's.
    std::fs::read_dir(&parent)
        .ok()?
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy() == name && e.path().is_dir())
        .map(|e| e.path().to_string_lossy().into_owned())
}

/// Data directories named by RUNNING agents, per agent — the scan's way to
/// reach a relocated install (`--config-dir ~/.claude-instances/second`) that
/// no path list could guess.
///
/// Cached for [`PROCESS_DIRS_TTL`]: a scan can fire on every file-system event,
/// and enumerating processes once per keystroke would be absurd.
#[must_use]
pub fn agent_data_dirs_from_processes() -> Vec<(String, String)> {
    /// `(agent, data_dir)` pairs, with the instant they were probed.
    type CachedDirs = std::sync::Mutex<Option<(std::time::Instant, Vec<(String, String)>)>>;
    static CACHE: std::sync::OnceLock<CachedDirs> = std::sync::OnceLock::new();
    let cell = CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((at, dirs)) = guard.as_ref() {
        if at.elapsed() < PROCESS_DIRS_TTL {
            return dirs.clone();
        }
    }
    let dirs: Vec<(String, String)> = probe_processes(current_os())
        .into_iter()
        .filter_map(|i| i.data_dir.map(|d| (i.agent, d)))
        .collect();
    let mut deduped = dirs;
    deduped.sort();
    deduped.dedup();
    *guard = Some((std::time::Instant::now(), deduped.clone()));
    deduped
}

/// How long process-derived data dirs stay fresh.
const PROCESS_DIRS_TTL: Duration = Duration::from_secs(5 * 60);

/// (2) Match running processes against the source registry.
///
/// Finds an agent installed where no path list looks — a bun/pnpm global, a
/// checkout run from source, a second app bundle under any name — and reads a
/// relocated data directory straight off the command line that relocated it.
fn probe_processes(os: Os) -> Vec<DetectedInstallation> {
    let procs = running_processes(os);
    if procs.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for spec in sources() {
        for p in &procs {
            let stem = exe_stem(&p.exe);
            // The executable itself, or a SCRIPT RUNNER whose first argument is
            // the agent's entry point (`node …/claude/cli.js`). The runner check
            // is deliberately narrow: matching any process whose first argument
            // merely looks like the binary tagged an app's unrelated helper
            // (`Claude.app/Contents/Helpers/disclaimer`) as the agent itself,
            // because a GUI helper is passed its own app's path.
            let via_runner = SCRIPT_RUNNERS.contains(&stem.as_str());
            let matches = spec.binaries.iter().any(|b| {
                stem == *b
                    || (via_runner && p.args.first().map(|a| exe_stem(a) == *b).unwrap_or(false))
            });
            if !matches {
                continue;
            }
            let binary_path = Path::new(&p.exe).is_absolute().then(|| p.exe.clone());
            out.push(DetectedInstallation {
                agent: spec.agent.to_string(),
                install_method: binary_path
                    .as_deref()
                    .map_or_else(|| "manual".to_string(), |b| classify_install_method(b, os)),
                binary_path,
                // A relocated dir the flag named, else the bundle's own.
                data_dir: data_dir_from_args(&p.args).or_else(|| bundle_data_dir(&p.exe)),
                // Deliberately not probed: `--version` on a RUNNING agent's
                // binary spawns a second copy of it.
                version: None,
                detected_via: vec!["process".to_string()],
            });
        }
    }
    out
}

fn os_dirs(spec: &SourceSpec, os: Os) -> &'static [&'static str] {
    match os {
        Os::Macos => spec.macos,
        Os::Linux => spec.linux,
        Os::Windows => spec.windows,
    }
}

fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok().filter(|s| !s.is_empty()))
}

fn env_or(name: &str, default: impl FnOnce() -> String) -> String {
    std::env::var(name)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(default)
}

/// Expand `$XDG_*`, `$HOME`, `$APPDATA`, `$LOCALAPPDATA`, and a leading `~`, then
/// make the path absolute (matching the TS `expandPath` + `resolve`).
fn expand_path(p: &str) -> String {
    let home = home_dir().unwrap_or_default();
    let mut s = p.to_string();
    s = s.replace(
        "$XDG_CONFIG_HOME",
        &env_or("XDG_CONFIG_HOME", || format!("{home}/.config")),
    );
    s = s.replace(
        "$XDG_DATA_HOME",
        &env_or("XDG_DATA_HOME", || format!("{home}/.local/share")),
    );
    s = s.replace("$LOCALAPPDATA", &env_or("LOCALAPPDATA", String::new));
    s = s.replace("$APPDATA", &env_or("APPDATA", String::new));
    s = s.replace("$HOME", &home);
    if let Some(rest) = s.strip_prefix('~') {
        s = format!("{home}{rest}");
    }
    // Resolve to absolute (relative to cwd) without requiring existence.
    let path = PathBuf::from(&s);
    if path.is_absolute() {
        s
    } else {
        std::env::current_dir()
            .map(|c| c.join(&path).to_string_lossy().into_owned())
            .unwrap_or(s)
    }
}

fn binary_lookup_dirs(os: Os) -> Vec<String> {
    let mut dirs: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let push = |d: String, dirs: &mut Vec<String>, seen: &mut BTreeSet<String>| {
        if !d.is_empty() && seen.insert(d.clone()) {
            dirs.push(d);
        }
    };
    let sep = if os == Os::Windows { ';' } else { ':' };
    if let Ok(path) = std::env::var("PATH") {
        for d in path.split(sep) {
            push(d.to_string(), &mut dirs, &mut seen);
        }
    }
    let home = home_dir().unwrap_or_default();
    let extra: Vec<String> = match os {
        Os::Macos => vec![
            "/opt/homebrew/bin".into(),
            "/usr/local/bin".into(),
            format!("{home}/.bun/bin"),
            format!("{home}/.volta/bin"),
            format!("{home}/.cargo/bin"),
            format!("{home}/.local/bin"),
            format!("{home}/.asdf/shims"),
            format!("{home}/.mise/shims"),
            format!("{home}/.npm-global/bin"),
            format!("{home}/.yarn/bin"),
        ],
        Os::Linux => vec![
            "/usr/local/bin".into(),
            "/usr/bin".into(),
            "/snap/bin".into(),
            "/var/lib/flatpak/exports/bin".into(),
            format!("{home}/.local/bin"),
            format!("{home}/.bun/bin"),
            format!("{home}/.cargo/bin"),
            format!("{home}/.nvm"),
        ],
        Os::Windows => vec![
            format!("{}\\Programs", env_or("LOCALAPPDATA", String::new)),
            format!("{home}\\scoop\\shims"),
            format!("{}\\npm", env_or("APPDATA", String::new)),
        ],
    };
    for d in extra {
        push(d, &mut dirs, &mut seen);
    }
    dirs
}

fn classify_install_method(bin_path: &str, os: Os) -> String {
    let p = bin_path;
    let m = if p.contains("/homebrew/") || p.contains("/Cellar/") {
        "homebrew"
    } else if p.contains("/.nvm/") || p.contains("/node_modules/") || p.contains("/.npm-global/") {
        "npm_global"
    } else if p.contains("/.pnpm/") || p.contains("/.pnpm-global/") {
        "pnpm_global"
    } else if p.contains("/.yarn/") {
        "yarn_global"
    } else if p.contains("/.bun/") {
        "bun_global"
    } else if p.contains("/.cargo/") {
        "cargo_install"
    } else if p.starts_with("/Applications/") || p.contains(".app/Contents/") {
        "app_bundle"
    } else if os == Os::Linux && p.starts_with("/snap/") {
        "snap"
    } else if os == Os::Linux && p.contains("/flatpak/") {
        "flatpak"
    } else {
        "manual"
    };
    m.to_string()
}

fn safe_version_probe(bin_path: &str) -> Option<String> {
    let out = run_command(bin_path, &["--version"], None, Duration::from_millis(1_500))?;
    let last = out.split_whitespace().last()?.to_string();
    if last.is_empty() {
        None
    } else {
        Some(last.chars().take(40).collect())
    }
}

fn read_json(path: &str) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// The `oauthAccount` fields from `~/.claude.json` a keychain hit can adopt.
#[derive(Default, Clone)]
struct ClaudeJsonAccount {
    stable_id: Option<String>,
    email: Option<String>,
    org: Option<String>,
    display: Option<String>,
}

fn probe_identities(os: Os) -> Vec<DetectedIdentity> {
    let mut ids: Vec<DetectedIdentity> = Vec::new();
    let home = home_dir().unwrap_or_default();

    // Claude Code — ~/.claude.json `oauthAccount` (desktop-app + recent CLI).
    // Probed FIRST so the keychain hit below can adopt this account when its
    // own blob is anonymous.
    let mut json_acct = ClaudeJsonAccount::default();
    let mut claude_configs = vec![format!("{home}/.claude.json")];
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            claude_configs.insert(0, format!("{dir}/.claude.json"));
        }
    }
    for candidate in &claude_configs {
        let Some(obj) = read_json(candidate) else {
            continue;
        };
        let Some(acct) = obj.get("oauthAccount") else {
            continue;
        };
        let stable_id = acct
            .get("accountUuid")
            .and_then(|v| v.as_str())
            .or_else(|| acct.get("organizationUuid").and_then(|v| v.as_str()));
        if let Some(stable_id) = stable_id {
            let sget = |k: &str| acct.get(k).and_then(|v| v.as_str()).map(str::to_string);
            let email = sget("emailAddress");
            let org = sget("organizationName");
            let display = sget("displayName");
            let billing = sget("billingType");
            let label = email
                .clone()
                .or_else(|| org.clone())
                .or_else(|| display.clone())
                .or_else(|| Some("Claude account".to_string()));
            json_acct = ClaudeJsonAccount {
                stable_id: Some(stable_id.to_string()),
                email: email.clone(),
                org: org.clone(),
                display: display.clone(),
            };
            ids.push(DetectedIdentity {
                provider: "anthropic".into(),
                provider_account_id: stable_id.to_string(),
                provider_account_label: label,
                account_email: email,
                account_org: org.or(billing),
                display_name: display,
                owner_scope: "unassigned".into(),
                detection_source: "claude_json_oauth".into(),
            });
            break;
        }
    }

    // Claude Code — macOS Keychain "Claude Code-credentials".
    if os == Os::Macos {
        if let Some(out) = run_command(
            "security",
            &[
                "find-generic-password",
                "-s",
                "Claude Code-credentials",
                "-w",
            ],
            None,
            Duration::from_millis(3_000),
        ) {
            if let Ok(body) = serde_json::from_str::<serde_json::Value>(out.trim()) {
                if let Some(oauth) = body.get("claudeAiOauth") {
                    if let Some(tok) = oauth.get("accessToken").and_then(|v| v.as_str()) {
                        // Identity for the keychain credentials, best first:
                        // 1. the blob's own account/org (newer CLI blobs);
                        // 2. ADOPT the ~/.claude.json `oauthAccount` — the login
                        //    flow writes the keychain tokens and that file
                        //    TOGETHER, so an anonymous blob (tokens + plan only)
                        //    belongs to that same login; same id → the dedupe
                        //    below collapses both probes into ONE account
                        //    instead of a useless plan-named row. (Never enrich
                        //    via provider APIs: Anthropic's terms restrict
                        //    Claude OAuth tokens to Claude Code itself.)
                        // 3. last resort: a sha256 HASH of the refresh token —
                        //    stable across access-token refreshes, and never a
                        //    slice of the token itself (that would ship live
                        //    secret material as an account id).
                        let email = oauth
                            .get("account")
                            .and_then(|a| a.get("email_address").or_else(|| a.get("email")))
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                            .or_else(|| json_acct.email.clone());
                        let org_name = oauth
                            .get("organization")
                            .and_then(|o| o.get("name"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                            .or_else(|| json_acct.org.clone());
                        let sub_type = oauth
                            .get("subscriptionType")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let stable_id = oauth
                            .get("account")
                            .and_then(|a| a.get("uuid"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                            .or_else(|| {
                                oauth
                                    .get("organization")
                                    .and_then(|o| o.get("uuid"))
                                    .and_then(|v| v.as_str())
                                    .map(str::to_string)
                            })
                            .or_else(|| json_acct.stable_id.clone())
                            .unwrap_or_else(|| {
                                use sha2::{Digest, Sha256};
                                let refresh = oauth
                                    .get("refreshToken")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(tok);
                                let hex = format!("{:x}", Sha256::digest(refresh.as_bytes()));
                                format!("kc_{}", &hex[..32])
                            });
                        let label = email
                            .clone()
                            .or_else(|| org_name.clone())
                            .or_else(|| Some("Claude account".to_string()));
                        ids.push(DetectedIdentity {
                            provider: "anthropic".into(),
                            provider_account_id: stable_id,
                            provider_account_label: label,
                            account_email: email,
                            account_org: org_name.or(sub_type),
                            display_name: json_acct.display.clone(),
                            owner_scope: "unassigned".into(),
                            detection_source: "claude_keychain".into(),
                        });
                    }
                }
            }
        }
    }

    // Codex auth.json — JWT id_token → email/sub/name/org.
    for candidate in [
        format!("{home}/.codex/auth.json"),
        format!("{home}/.config/codex/auth.json"),
    ] {
        let Some(obj) = read_json(&candidate) else {
            continue;
        };
        let tokens = obj.get("tokens");
        let jwt = tokens
            .and_then(|t| t.get("id_token"))
            .and_then(|v| v.as_str());
        let mut email = None;
        let mut sub = None;
        let mut name = None;
        let mut org = None;
        if let Some(jwt) = jwt {
            if let Some(claims) = decode_jwt_claims(jwt) {
                email = claims
                    .get("email")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                sub = claims
                    .get("sub")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                name = claims
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                if let Some(oai) = claims.get("https://api.openai.com/auth") {
                    org = oai
                        .get("organization_id")
                        .and_then(|v| v.as_str())
                        .or_else(|| oai.get("chatgpt_plan_type").and_then(|v| v.as_str()))
                        .map(str::to_string);
                }
            }
        }
        let account_id = tokens
            .and_then(|t| t.get("account_id"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let pid = account_id.or_else(|| sub.clone()).or_else(|| email.clone());
        if let Some(pid) = pid {
            ids.push(DetectedIdentity {
                provider: "openai".into(),
                provider_account_id: pid,
                provider_account_label: email.clone(),
                account_email: email,
                account_org: org,
                display_name: name,
                owner_scope: "unassigned".into(),
                detection_source: "codex_auth_json".into(),
            });
        }
    }

    // Gemini oauth_creds.json — email as id.
    for candidate in [
        format!("{home}/.gemini/oauth_creds.json"),
        format!("{home}/.config/gemini/oauth_creds.json"),
    ] {
        let Some(obj) = read_json(&candidate) else {
            continue;
        };
        let email = obj
            .get("email")
            .and_then(|v| v.as_str())
            .or_else(|| {
                obj.get("token")
                    .and_then(|t| t.get("email"))
                    .and_then(|v| v.as_str())
            })
            .map(str::to_string);
        if let Some(email) = email {
            ids.push(DetectedIdentity {
                provider: "google".into(),
                provider_account_id: email.clone(),
                provider_account_label: Some(email.clone()),
                account_email: Some(email),
                account_org: None,
                display_name: None,
                owner_scope: "unassigned".into(),
                detection_source: "gemini_oauth_creds".into(),
            });
        }
    }

    // Cursor — the signed-in account, from the SQLite store its editor actually
    // writes. `storage.json` (probed below, kept for older builds) holds no
    // auth keys on a current install: they live in `state.vscdb`'s ItemTable as
    // plain values. Until this probe existed every Cursor session on the
    // machine landed unattributed — measured on prod, 3,253 of 3,253 messages
    // with no account.
    for db in [
        format!("{home}/Library/Application Support/Cursor/User/globalStorage/state.vscdb"),
        format!("{home}/.config/Cursor/User/globalStorage/state.vscdb"),
        format!(
            "{}/Cursor/User/globalStorage/state.vscdb",
            env_or("APPDATA", String::new)
        ),
    ] {
        if !Path::new(&db).is_file() {
            continue;
        }
        // Exactly these keys. `cursorAuth/accessToken` and `refreshToken` are
        // neighbours in this table; nothing sweeps the prefix.
        let vals = crate::cursor::read_item_table(
            &db,
            &["cursorAuth/cachedEmail", "cursorAuth/cachedSignUpType"],
        );
        let Some(email) = vals.get("cursorAuth/cachedEmail") else {
            continue;
        };
        // The account id is the e-mail, lowercased. Cursor also stashes a
        // numeric `dashboardUserId`, but only inside a 360 KB reactive-storage
        // blob: if that field ever moves, the same account would come back
        // under a second id and the identity would DUPLICATE. A dedicated
        // top-level key cannot drift that way.
        ids.push(DetectedIdentity {
            provider: "cursor".into(),
            provider_account_id: email.to_lowercase(),
            provider_account_label: Some(email.clone()),
            account_email: Some(email.clone()),
            account_org: None,
            display_name: vals.get("cursorAuth/cachedSignUpType").cloned(),
            owner_scope: "unassigned".into(),
            detection_source: "cursor_item_table".into(),
        });
        break;
    }

    // Cursor (legacy) — globalStorage/storage.json `cursorAuth/*`.
    for candidate in [
        format!("{home}/Library/Application Support/Cursor/User/globalStorage/storage.json"),
        format!("{home}/.config/Cursor/User/globalStorage/storage.json"),
    ] {
        let Some(obj) = read_json(&candidate) else {
            continue;
        };
        let Some(map) = obj.as_object() else { continue };
        for (k, v) in map {
            if k.starts_with("cursorAuth") {
                if let Some(s) = v.as_str() {
                    if let Ok(auth) = serde_json::from_str::<serde_json::Value>(s) {
                        let sub = auth.get("sub").and_then(|v| v.as_str());
                        let email = auth.get("email").and_then(|v| v.as_str());
                        if sub.is_some() || email.is_some() {
                            let pid = sub.or(email).unwrap().to_string();
                            ids.push(DetectedIdentity {
                                provider: "cursor".into(),
                                provider_account_id: pid,
                                provider_account_label: email.map(str::to_string),
                                account_email: email.map(str::to_string),
                                account_org: None,
                                display_name: None,
                                owner_scope: "unassigned".into(),
                                detection_source: "cursor_global_storage".into(),
                            });
                            break;
                        }
                    }
                }
            }
        }
    }

    ids
}

/// Decode a JWT's payload (the second dot-segment) as base64url → JSON claims.
fn decode_jwt_claims(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64url_decode(payload)?;
    serde_json::from_slice(&bytes).ok()
}

/// Minimal base64url decoder (alphabet `A-Za-z0-9-_`, padding optional). Kept
/// dependency-free — the collector avoids extra crates.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &c in input.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = val(c)?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn dedupe_installs(list: Vec<DetectedInstallation>) -> Vec<DetectedInstallation> {
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashMap<String, DetectedInstallation> = HashMap::new();
    for i in list {
        let k = format!(
            "{}|{}|{}",
            i.agent,
            i.binary_path.clone().unwrap_or_default(),
            i.data_dir.clone().unwrap_or_default()
        );
        match seen.get_mut(&k) {
            None => {
                order.push(k.clone());
                seen.insert(k, i);
            }
            Some(prev) => {
                for v in i.detected_via {
                    if !prev.detected_via.contains(&v) {
                        prev.detected_via.push(v);
                    }
                }
                if prev.version.is_none() {
                    prev.version = i.version;
                }
            }
        }
    }
    order.into_iter().filter_map(|k| seen.remove(&k)).collect()
}

fn dedupe_identities(list: Vec<DetectedIdentity>) -> Vec<DetectedIdentity> {
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashMap<String, DetectedIdentity> = HashMap::new();
    for i in list {
        let k = format!("{}|{}", i.provider, i.provider_account_id);
        match seen.entry(k.clone()) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                order.push(k);
                slot.insert(i);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                // Same account seen by several probes (e.g. .claude.json +
                // keychain): keep the first entry but fill any identity field
                // it was missing, so probe order never costs a detected
                // email/org/name.
                let prev = slot.get_mut();
                if prev.provider_account_label.is_none() {
                    prev.provider_account_label = i.provider_account_label;
                }
                if prev.account_email.is_none() {
                    prev.account_email = i.account_email;
                }
                if prev.account_org.is_none() {
                    prev.account_org = i.account_org;
                }
                if prev.display_name.is_none() {
                    prev.display_name = i.display_name;
                }
            }
        }
    }
    order.into_iter().filter_map(|k| seen.remove(&k)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_install_method_cases() {
        assert_eq!(
            classify_install_method("/opt/homebrew/bin/claude", Os::Macos),
            "homebrew"
        );
        assert_eq!(
            classify_install_method("/home/x/.cargo/bin/zed", Os::Linux),
            "cargo_install"
        );
        assert_eq!(
            classify_install_method("/snap/bin/ollama", Os::Linux),
            "snap"
        );
        assert_eq!(
            classify_install_method("/usr/local/bin/codex", Os::Linux),
            "manual"
        );
        assert_eq!(
            classify_install_method("/Applications/Cursor.app/Contents/MacOS/cursor", Os::Macos),
            "app_bundle"
        );
    }

    #[test]
    fn jwt_claims_decode() {
        // {"email":"a@b.com","sub":"user_1"} base64url, no padding.
        let payload = "eyJlbWFpbCI6ImFAYi5jb20iLCJzdWIiOiJ1c2VyXzEifQ";
        let jwt = format!("hdr.{payload}.sig");
        let claims = decode_jwt_claims(&jwt).unwrap();
        assert_eq!(claims["email"], "a@b.com");
        assert_eq!(claims["sub"], "user_1");
    }

    #[test]
    fn dedupe_installs_merges_detected_via() {
        let list = vec![
            DetectedInstallation {
                agent: "claude_code".into(),
                install_method: "manual".into(),
                binary_path: None,
                data_dir: Some("/x/.claude".into()),
                version: None,
                detected_via: vec!["known_path".into()],
            },
            DetectedInstallation {
                agent: "claude_code".into(),
                install_method: "manual".into(),
                binary_path: None,
                data_dir: Some("/x/.claude".into()),
                version: Some("1.0".into()),
                detected_via: vec!["binary_walk".into()],
            },
        ];
        let out = dedupe_installs(list);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].detected_via, vec!["known_path", "binary_walk"]);
        assert_eq!(out[0].version.as_deref(), Some("1.0"));
    }

    #[test]
    fn discover_smoke_runs_and_stays_deduped() {
        // A real discovery pass on the CI runner (M2 AC "discovery smoke"). It is
        // best-effort — it must never panic, and the output must hold the dedupe
        // invariants regardless of what (if anything) is installed here.
        let out = discover(&DiscoveryOptions::default());
        let mut install_keys = std::collections::HashSet::new();
        for i in &out.installations {
            let k = format!(
                "{}|{}|{}",
                i.agent,
                i.binary_path.clone().unwrap_or_default(),
                i.data_dir.clone().unwrap_or_default()
            );
            assert!(
                install_keys.insert(k),
                "duplicate install key: {:?}",
                i.agent
            );
        }
        let mut id_keys = std::collections::HashSet::new();
        for i in &out.identities {
            let k = format!("{}|{}", i.provider, i.provider_account_id);
            assert!(id_keys.insert(k), "duplicate identity key");
        }
    }

    #[test]
    fn dedupe_identities_keeps_first_per_key() {
        let mk = |label: &str| DetectedIdentity {
            provider: "anthropic".into(),
            provider_account_id: "acc_1".into(),
            provider_account_label: Some(label.into()),
            account_email: None,
            account_org: None,
            display_name: None,
            owner_scope: "unassigned".into(),
            detection_source: "claude_keychain".into(),
        };
        let out = dedupe_identities(vec![mk("first"), mk("second")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].provider_account_label.as_deref(), Some("first"));
    }

    #[test]
    fn dedupe_identities_fills_missing_fields_from_later_probes() {
        // .claude.json probe knows the email; the keychain probe of the SAME
        // account knows the org — the merged row keeps both (first entry wins
        // per field, gaps fill from later duplicates).
        let first = DetectedIdentity {
            provider: "anthropic".into(),
            provider_account_id: "acc_1".into(),
            provider_account_label: Some("a@x.com".into()),
            account_email: Some("a@x.com".into()),
            account_org: None,
            display_name: None,
            owner_scope: "unassigned".into(),
            detection_source: "claude_json_oauth".into(),
        };
        let second = DetectedIdentity {
            provider: "anthropic".into(),
            provider_account_id: "acc_1".into(),
            provider_account_label: Some("other".into()),
            account_email: None,
            account_org: Some("goldsky".into()),
            display_name: Some("Aram".into()),
            owner_scope: "unassigned".into(),
            detection_source: "claude_keychain".into(),
        };
        let out = dedupe_identities(vec![first, second]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].provider_account_label.as_deref(), Some("a@x.com"));
        assert_eq!(out[0].account_email.as_deref(), Some("a@x.com"));
        assert_eq!(out[0].account_org.as_deref(), Some("goldsky"));
        assert_eq!(out[0].display_name.as_deref(), Some("Aram"));
    }
}

#[cfg(test)]
mod process_probe_tests {
    use super::*;

    #[test]
    fn exe_stem_reads_the_file_name_case_folded_and_de_exed() {
        assert_eq!(exe_stem("/opt/homebrew/bin/claude"), "claude");
        assert_eq!(exe_stem("C:\\Program Files\\Codex\\codex.EXE"), "codex");
        assert_eq!(
            exe_stem("/Applications/Claude.app/Contents/MacOS/Claude"),
            "claude"
        );
        assert_eq!(exe_stem(""), "");
    }

    #[test]
    fn a_relocated_data_dir_is_read_off_the_command_line() {
        let dir = std::env::temp_dir().join(format!("modelstat-relocated-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_string_lossy().into_owned();

        // `--flag value` and `--flag=value` both.
        assert_eq!(
            data_dir_from_args(&["--config-dir".into(), d.clone()]),
            Some(d.clone())
        );
        assert_eq!(
            data_dir_from_args(&[format!("--user-data-dir={d}")]),
            Some(d.clone())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The probe reads command lines, which routinely carry secrets. Nothing
    /// leaves it but a path it recognised AND resolved to a real directory.
    #[test]
    fn nothing_but_a_real_directory_escapes_the_command_line() {
        // An unrecognised flag's value is never taken, however path-like.
        assert_eq!(
            data_dir_from_args(&["--api-key".into(), "sk-live-not-a-path".into()]),
            None
        );
        // A recognised flag pointing nowhere is dropped, not guessed at.
        assert_eq!(
            data_dir_from_args(&["--config-dir".into(), "/no/such/directory".into()]),
            None
        );
        // A secret sitting next to a recognised flag is not confused for one.
        assert_eq!(
            data_dir_from_args(&["--token".into(), "ghp_0123456789abcdef".into()]),
            None
        );
        assert_eq!(data_dir_from_args(&[]), None);
    }

    #[test]
    fn a_bundle_outside_application_support_resolves_to_nothing() {
        // Nothing is installed under this name, so no directory is invented.
        assert_eq!(
            bundle_data_dir("/Applications/Definitely Not Installed.app/Contents/MacOS/x"),
            None
        );
        // A plain binary is not a bundle at all.
        assert_eq!(bundle_data_dir("/usr/local/bin/claude"), None);
    }

    /// Running the probe must never panic or hang, whatever this machine has
    /// running, and it must never invent an install for an agent that owns no
    /// binaries.
    #[test]
    fn probing_this_machine_is_safe_and_well_formed() {
        for i in probe_processes(current_os()) {
            assert_eq!(i.detected_via, vec!["process".to_string()]);
            assert!(
                sources().iter().any(|s| s.agent == i.agent),
                "only registry agents are reported"
            );
            if let Some(d) = &i.data_dir {
                assert!(Path::new(d).is_dir(), "a reported data dir exists: {d}");
            }
        }
    }

    #[test]
    fn the_cached_accessor_is_stable_and_returns_real_dirs() {
        let a = agent_data_dirs_from_processes();
        let b = agent_data_dirs_from_processes();
        assert_eq!(a, b, "second call is served from the cache");
        for (agent, dir) in a {
            assert!(!agent.is_empty());
            assert!(Path::new(&dir).is_dir(), "{dir} exists");
        }
    }
}
