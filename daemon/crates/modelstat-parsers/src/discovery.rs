//! Source Discovery Engine.
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
    /// The handles other systems know the machine's PERSON by — see
    /// [`DetectedHandle`].
    pub handles: Vec<DetectedHandle>,
}

/// A handle the machine's person is known by elsewhere: a GitHub login the
/// gh CLI is signed in with, the email and name git commits as. A handle is a
/// fact about the PERSON who paired this device (the server folds it onto
/// their profile), never about a session — which is why it travels beside the
/// identities and not inside a transcript. `provider` is an open slug (`github`,
/// a GitHub Enterprise host, `email`, …), whatever the record names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedHandle {
    pub provider: String,
    pub handle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub detection_source: String,
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
            agent: "bb",
            macos: &["~/.bb"],
            linux: &["~/.bb"],
            windows: &["~/.bb"],
            data_dir_env: &[],
            binaries: &["bb"],
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

/// Directory names no transcript hunt may descend into. ONE list, consulted by
/// every walk, so a new call site cannot honour half of it.
///
/// Two reasons, and the second is the load-bearing one:
///
///   * COST — vendor caches, blob stores and VM images hold no transcripts and
///     plenty of gigabytes.
///   * CONSENT — macOS guards a handful of directories behind TCC, so merely
///     OPENING one makes the system interrupt the user with "modelstat would
///     like to access your Contacts". None of them can hold an agent's
///     transcripts, so the honest thing is never to open them.
///
/// The user's own `Desktop`, `Documents`, `Downloads`, `Pictures` and `Music`
/// are guarded the same way and are deliberately NOT named here: they live
/// directly under a home, and a home's children are excluded by a positive rule
/// instead — see [`Children::Hidden`].
pub const SKIP_DIRS: &[&str] = &[
    // Cost.
    "node_modules",
    "blob_storage",
    "Cache",
    "Code Cache",
    "GPUCache",
    "CachedData",
    "Crashpad",
    "logs",
    "claude-code-vm",
    "Partitions",
    "Service Worker",
    "IndexedDB",
    "Local Storage",
    // A deleted `~/.sometool` keeps its transcripts, and reporting it as a live
    // install is a claim the user has already withdrawn. Both spellings, for the
    // same reason `application_data_roots` returns every platform's paths: a
    // `cfg!` here would be a claim about the machine. macOS trashes to
    // `~/.Trash`, Linux to `~/.local/share/Trash`.
    ".Trash",
    "Trash",
    // Consent — Apple's private stores, which sit inside an application-data
    // root among ordinary app directories.
    "AddressBook",
    "CallHistoryDB",
    "CallHistoryTransactions",
    "com.apple.TCC",
    "Knowledge",
    "MobileSync",
    "icdd",
];

/// Which children of a scan root the signature probe may open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Children {
    /// Hidden directories only — the rule for a HOME.
    ///
    /// An agent's own store is a dotdir without exception (`~/.claude`,
    /// `~/.codex`, `~/.gemini`, `~/.aider`), while a home's VISIBLE children are
    /// the human's own folders. On macOS `Desktop`, `Documents`, `Downloads`,
    /// `Pictures` and `Music` are guarded by TCC, so a probe that merely opens
    /// one makes the system interrupt the user with a consent dialog — for a
    /// walk that was looking for `.jsonl` files, on every discovery pass,
    /// forever.
    ///
    /// A POSITIVE rule rather than a longer skip list on purpose. A list of
    /// guarded names is a claim about which directories macOS protects this
    /// year, and it is outgrown by the next release, by a synced `~/Dropbox`,
    /// by whatever the user called their photo folder. "Hidden only" is a claim
    /// about where agents keep transcripts, which is ours to know and does not
    /// rot.
    Hidden,
    /// Every directory — the rule for an application-data root, whose children
    /// ARE application data directories and carry ordinary visible names
    /// (`Cursor`, `Claude`, whatever a fork calls itself).
    Any,
}

/// Where applications keep their data, per platform — NOT a list of app names.
///
/// All three platforms are returned regardless of which one we are on. A
/// `cfg!` here would be a claim about the machine, and it is wrong often enough
/// to matter: translation layers, mounted volumes and test harnesses all put a
/// foreign layout under a real home. A path for the wrong platform simply does
/// not exist, which costs one `read_dir` that fails.
#[must_use]
pub fn application_data_roots(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join("Library/Application Support"),
        home.join(".config"),
        home.join("AppData/Roaming"),
    ]
}

/// Every directory this device might keep `agent`'s data in, for `home`.
///
/// ONE list, consulted by everything that looks for an agent's files. It used to
/// be two: `sources()` honoured `CODEX_HOME`, `PI_HOME`, `OMP_HOME` and
/// `XDG_CONFIG_HOME`, while the scan's own job discovery hard-coded `~/.codex`
/// and friends — so a user who relocated codex had the install REPORTED and its
/// sessions never read, which is the worst of both (the dashboard says the tool
/// is there and it never spends a token).
///
/// Three sources, in the order they were learned: the known per-platform paths,
/// the environment variables that relocate them, and the data directory a
/// RUNNING instance names on its own command line — the last being the only way
/// to reach an install nothing on disk points at.
#[must_use]
pub fn data_dir_candidates_in(home: &Path, agent: &str) -> Vec<String> {
    data_dir_candidates_from(home, agent, &agent_data_dirs_from_processes())
}

/// [`data_dir_candidates_in`] with the running-process reading supplied.
///
/// The probe is a parameter because it is the one input that is not a function
/// of `home`: it reports absolute paths on THIS machine, which is exactly the
/// point in production (a relocated install can live anywhere) and exactly wrong
/// for a caller working over a root of its own. A test passing `&[]` gets a
/// discovery scoped to the tree it built, instead of one that finds whatever the
/// developer happens to have open.
#[must_use]
pub fn data_dir_candidates_from(
    home: &Path,
    agent: &str,
    process_dirs: &[(String, String)],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut add = |c: String| {
        if !c.is_empty() && seen.insert(c.clone()) {
            out.push(c);
        }
    };
    for spec in sources().iter().filter(|s| s.agent == agent) {
        // Every platform's paths, for the same reason `application_data_roots`
        // returns all three.
        for raw in spec.macos.iter().chain(spec.linux).chain(spec.windows) {
            add(expand_path_with_home(home, raw));
        }
        for env in spec.data_dir_env {
            if let Ok(v) = std::env::var(env) {
                add(v);
            }
        }
    }
    for (probed_agent, dir) in process_dirs {
        if probed_agent == agent {
            add(dir.clone());
        }
    }
    out
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

    // (4) file signatures — a transcript store nobody enumerated.
    if !options.skip.contains(&Strategy::FileSignatures) {
        installations.extend(probe_file_signatures());
    }

    // (6) application registry (macOS `system_profiler`) — DROPPED (§22).

    // Identity probes — best-effort, filesystem + keychain.
    identities.extend(probe_identities(os));

    DiscoveryOutput {
        installations: dedupe_installs(installations),
        identities: dedupe_identities(identities),
        handles: discover_handles(),
    }
}

/// The handle probes alone — what the machine's person is called elsewhere.
///
/// Read on the install clock, not the identity clock: a GitHub login or a git
/// identity changes about as often as the tools do, and each reading spawns
/// `git` twice. Best-effort: a machine without gh or git simply reports none.
#[must_use]
pub fn discover_handles() -> Vec<DetectedHandle> {
    let home = home_dir().unwrap_or_default();
    let mut out = probe_gh_handles(&home);
    out.extend(probe_git_handles());
    dedupe_handles(out)
}

/// The gh CLI's signed-in logins, from its `hosts.yml` — never its tokens.
///
/// The file is `$GH_CONFIG_DIR/hosts.yml`, else `$XDG_CONFIG_HOME/gh/hosts.yml`,
/// else `~/.config/gh/hosts.yml` (`%APPDATA%\GitHub CLI\hosts.yml` on Windows).
fn probe_gh_handles(home: &str) -> Vec<DetectedHandle> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(dir) = std::env::var("GH_CONFIG_DIR") {
        if !dir.is_empty() {
            candidates.push(format!("{dir}/hosts.yml"));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            candidates.push(format!("{xdg}/gh/hosts.yml"));
        }
    }
    if !home.is_empty() {
        candidates.push(format!("{home}/.config/gh/hosts.yml"));
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.is_empty() {
            candidates.push(format!("{appdata}/GitHub CLI/hosts.yml"));
        }
    }
    for path in candidates {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let found = handles_from_gh_hosts(&raw);
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

/// The logins a gh `hosts.yml` names, one handle per host. `github.com` is the
/// provider `github`; any other host (GitHub Enterprise) is its own provider,
/// named as the file names it. Only the login travels: the `oauth_token` beside
/// it is never read into anything.
fn handles_from_gh_hosts(raw: &str) -> Vec<DetectedHandle> {
    let Ok(doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(raw) else {
        return Vec::new();
    };
    let Some(hosts) = doc.as_mapping() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (host, entry) in hosts {
        let Some(host) = host.as_str().map(str::trim).filter(|h| !h.is_empty()) else {
            continue;
        };
        let provider = if host.eq_ignore_ascii_case("github.com") {
            "github".to_string()
        } else {
            host.to_lowercase()
        };
        // The active login, else every login the file remembers.
        let mut logins: Vec<String> = Vec::new();
        if let Some(user) = entry.get("user").and_then(|v| v.as_str()) {
            logins.push(user.to_string());
        }
        if let Some(users) = entry.get("users").and_then(|v| v.as_mapping()) {
            for (login, _) in users {
                if let Some(l) = login.as_str() {
                    logins.push(l.to_string());
                }
            }
        }
        for login in logins {
            let login = login.trim();
            if login.is_empty() {
                continue;
            }
            out.push(DetectedHandle {
                provider: provider.clone(),
                handle: login.to_string(),
                display_name: None,
                email: None,
                detection_source: "gh_cli_hosts".into(),
            });
        }
    }
    out
}

/// The identity git commits as on this machine: `user.email` (the handle) and
/// `user.name`, read through `git config --global` so includes and conditional
/// includes resolve as git resolves them.
fn probe_git_handles() -> Vec<DetectedHandle> {
    let read = |key: &str| {
        run_command(
            "git",
            &["config", "--global", "--get", key],
            None,
            Duration::from_secs(3),
        )
    };
    git_identity_handles(read("user.email"), read("user.name"))
}

/// A git identity as a handle: the email, lower-cased and trimmed, with the
/// name beside it. No email, no handle — a name alone names nobody.
fn git_identity_handles(email: Option<String>, name: Option<String>) -> Vec<DetectedHandle> {
    let email = email
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty() && e.contains('@'));
    let Some(email) = email else {
        return Vec::new();
    };
    let display_name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    vec![DetectedHandle {
        provider: "email".into(),
        handle: email.clone(),
        display_name,
        email: Some(email),
        detection_source: "git_config".into(),
    }]
}

/// Same handle seen by several probes: keep the first, fill the gaps.
fn dedupe_handles(list: Vec<DetectedHandle>) -> Vec<DetectedHandle> {
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashMap<String, DetectedHandle> = HashMap::new();
    for h in list {
        let k = format!("{}|{}", h.provider, h.handle.to_lowercase());
        match seen.entry(k.clone()) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                order.push(k);
                slot.insert(h);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let prev = slot.get_mut();
                if prev.display_name.is_none() {
                    prev.display_name = h.display_name;
                }
                if prev.email.is_none() {
                    prev.email = h.email;
                }
            }
        }
    }
    order.into_iter().filter_map(|k| seen.remove(&k)).collect()
}

/// The identity probes alone — which accounts are logged in on this device.
///
/// Split out of [`discover`] because the two halves answer questions on
/// completely different clocks. WHO is logged in changes whenever somebody runs
/// `claude login`, and the window each account's `observed_since` measures is
/// only correct if every reading updates it — so the daemon reads this on every
/// heartbeat. WHICH TOOLS are installed does not meaningfully change between two
/// heartbeats, and asking costs a full process listing, a `--version`
/// subprocess per binary found, and a filesystem sweep.
#[must_use]
pub fn discover_identities() -> Vec<DetectedIdentity> {
    dedupe_identities(probe_identities(current_os()))
}

/// How far below a candidate directory to look for a transcript before giving
/// up. Four levels covers every layout observed — codex nests deepest at
/// `sessions/<y>/<m>/<d>/rollout-*.jsonl`.
const FILE_SIGNATURE_MAX_DEPTH: usize = 4;

/// (4) File signatures — transcript stores nobody enumerated.
///
/// Every other strategy needs the tool to be in [`sources()`] first: a known
/// path, a known binary name, a known process. So a tool that ships tomorrow is
/// invisible until somebody adds it here and cuts a release, and that release
/// cadence is the thing this strategy exists to remove.
///
/// The signature is STRUCTURAL and deliberately thin — a HIDDEN directory under
/// the user's home, or any directory under an application-data root, that holds
/// `.jsonl` files a few levels down. That is the shape every JSONL agent's
/// store has, and it is all we can honestly claim to recognise. Nothing is
/// parsed and nothing is read; this reports "there is a transcript-shaped store
/// here, called this", which is exactly the fact a human needs to decide whether
/// a parser is worth writing.
///
/// The home's VISIBLE children are never opened — see [`Children::Hidden`] for
/// why that rule is the fix for a probe that used to make macOS ask the user for
/// their photos every ten seconds.
///
/// The agent name is the DIRECTORY's own name, because that is the only name
/// anything states. A single leading `.` is dropped — the dotfile convention is
/// filesystem grammar, not part of what the tool is called, and reporting `.foo`
/// and `foo` as two tools would be worse than either. The wire carries `agent`
/// as a plain string precisely so a name no build of ours knows can ride it.
///
/// Directories a known source already claims are left out: they are reported by
/// the strategies that understand them, and a second entry under a different
/// name would split one install in two.
fn probe_file_signatures() -> Vec<DetectedInstallation> {
    home_dir().map_or_else(Vec::new, |h| probe_file_signatures_in(Path::new(&h)))
}

/// [`probe_file_signatures`] over an explicit home.
///
/// The home is injected so a test can assert the ROOT WIRING — which rule each
/// root is walked under, which is the whole of the consent fix — over a tree it
/// built, instead of over whatever the developer happens to keep in their own
/// home.
fn probe_file_signatures_in(home: &Path) -> Vec<DetectedInstallation> {
    let app_roots = application_data_roots(home);
    // Directories another strategy already owns. The application-data roots are
    // in here alongside the known source paths, because `~/.config` is BOTH a
    // root of its own and a dotdir under the home: without this it is walked as
    // a root and then reported a second time as an agent literally called
    // "config".
    let claimed: BTreeSet<String> = sources()
        .iter()
        .flat_map(|s| s.macos.iter().chain(s.linux).chain(s.windows))
        .map(|raw| expand_path_with_home(home, raw))
        .chain(app_roots.iter().map(|r| r.to_string_lossy().into_owned()))
        .collect();

    let mut roots: Vec<(PathBuf, Children)> =
        app_roots.into_iter().map(|r| (r, Children::Any)).collect();
    roots.push((home.to_path_buf(), Children::Hidden));
    roots
        .iter()
        .flat_map(|(root, children)| file_signature_installs(root, *children, &claimed))
        .collect()
}

/// The transcript-shaped children of one directory, as installations.
fn file_signature_installs(
    root: &Path,
    children: Children,
    claimed: &BTreeSet<String>,
) -> Vec<DetectedInstallation> {
    let mut out = Vec::new();
    for candidate in child_dirs(root) {
        let name = candidate
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        // Decided from the NAME, before anything opens the directory — the
        // whole point is that a guarded folder is never read, and a consent
        // dialog fires on the open, not on what the open finds.
        if children == Children::Hidden && !name.starts_with('.') {
            continue;
        }
        let agent = name.strip_prefix('.').unwrap_or(name);
        if agent.is_empty() || SKIP_DIRS.contains(&name) {
            continue;
        }
        let path = candidate.to_string_lossy().into_owned();
        if claimed.contains(&path) || !holds_transcripts(&candidate, FILE_SIGNATURE_MAX_DEPTH) {
            continue;
        }
        out.push(DetectedInstallation {
            agent: agent.to_string(),
            install_method: "unknown".to_string(),
            binary_path: None,
            data_dir: Some(path),
            version: None,
            detected_via: vec!["file_signatures".to_string()],
        });
    }
    out
}

/// Does `dir` hold a `.jsonl` file within `depth` levels? Stops at the first
/// one — the question is whether the shape is there, not how much of it.
fn holds_transcripts(dir: &Path, depth: usize) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ty) = entry.file_type() else { continue };
        if ty.is_file() {
            if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                return true;
            }
        } else if ty.is_dir() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !SKIP_DIRS.contains(&name.as_str()) {
                subdirs.push(path);
            }
        }
    }
    depth > 0 && subdirs.iter().any(|d| holds_transcripts(d, depth - 1))
}

/// Immediate subdirectories of `dir` (empty when it is not readable).
fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
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
    expand_path_with_home(Path::new(&home_dir().unwrap_or_default()), p)
}

/// [`expand_path`] against an explicit home.
///
/// The home is injected so a caller working over a test root — or any home that
/// is not this process's — resolves every home-relative form inside it.
fn expand_path_with_home(home: &Path, p: &str) -> String {
    let home = home.to_string_lossy().into_owned();
    // `$APPDATA`, `$LOCALAPPDATA` and the XDG pair are PLATFORM BASE
    // DIRECTORIES: they say where *this user's* home keeps application data. So
    // they are read from the environment only when the home being expanded IS
    // this user's, and derived from the given home otherwise. Without that split
    // a caller working over an injected root asks for `<root>/AppData/Roaming`
    // and gets the real `%APPDATA%` — which on Windows is always set, so the
    // root it was handed is quietly ignored.
    //
    // The per-agent relocation variables (`CODEX_HOME`, `PI_HOME`, …) are a
    // different thing and keep winning unconditionally: those name one tool's
    // absolute directory, not a base the home derives.
    let is_this_users_home = home_dir().is_some_and(|real| real == home);
    let base = |var: &str, derived: String| {
        if is_this_users_home {
            env_or(var, || derived)
        } else {
            derived
        }
    };
    let mut s = p.to_string();
    s = s.replace(
        "$XDG_CONFIG_HOME",
        &base("XDG_CONFIG_HOME", format!("{home}/.config")),
    );
    s = s.replace(
        "$XDG_DATA_HOME",
        &base("XDG_DATA_HOME", format!("{home}/.local/share")),
    );
    // An unset `%APPDATA%` falls back to what the variable MEANS rather than to
    // the empty string, which used to turn `$APPDATA/Cursor` into the absolute
    // `/Cursor` — a path on nobody's machine that silently probed nothing.
    s = s.replace(
        "$LOCALAPPDATA",
        &base("LOCALAPPDATA", format!("{home}/AppData/Local")),
    );
    s = s.replace(
        "$APPDATA",
        &base("APPDATA", format!("{home}/AppData/Roaming")),
    );
    s = s.replace("$HOME", &home);
    if let Some(rest) = s.strip_prefix('~') {
        s = format!("{home}{rest}");
    }
    // Resolve to absolute (relative to cwd) without requiring existence.
    let path = PathBuf::from(&s);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().map_or(path.clone(), |c| c.join(&path))
    };
    // Re-collect through `components()` so the separators are the platform's.
    //
    // The templates above are written with `/`, which Windows accepts but does
    // not produce — and callers join onto these with `PathBuf`, which does. Two
    // spellings of one directory then read as two: the same transcript is
    // discovered under both, deduped under neither, and parsed and uploaded
    // several times a cycle. Its scan cursor is keyed by that string too, so the
    // second spelling never sees the first's progress.
    path.components()
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned()
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

    // Key-based providers configured inside a multi-provider agent
    // (pi/omp). Their spend was 100% unattributable — zhipu 60% and xai
    // 31% of events had no account — because a key has no OAuth login to
    // probe. Only a fingerprint of the key travels.
    ids.extend(probe_provider_key_identities(&home));

    ids
}

/// Providers whose account is an opaque API KEY rather than an OAuth login, as
/// configured inside a multi-provider agent (pi/omp keeps a `providers:` block).
///
/// Attribution needs to know WHOSE account paid, and for a key-based provider
/// the key IS the account. So the key is fingerprinted here and only the
/// fingerprint travels: a SHA-256 prefix, plus the last four characters as a
/// human-recognisable label, in the manner of a card. The key itself never
/// leaves this function — not to the wire, not to a log, not to the struct.
///
/// Rotating a key therefore reads as a new account. That is the honest
/// outcome: nothing on the machine ties the old key to the new one.
///
/// Deliberately shape-agnostic. The `providers:` block exists in a real config,
/// but its nesting under each provider was NOT observable on the machine this
/// was written on (that install configures no keys), so rather than guess a
/// schema this walks the whole subtree and pairs any provider-ish name with any
/// key-shaped string beneath it. A layout we guessed wrong yields nothing
/// rather than something false.
fn probe_provider_key_identities(home: &str) -> Vec<DetectedIdentity> {
    let mut out = Vec::new();
    for rel in [".pi/agent/config.yml", ".omp/agent/config.yml"] {
        let path = format!("{home}/{rel}");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&raw) else {
            continue;
        };
        let Some(providers) = doc.get("providers") else {
            continue;
        };
        let Some(map) = providers.as_mapping() else {
            continue;
        };
        for (name, sub) in map {
            let Some(name) = name.as_str().map(str::to_lowercase) else {
                continue;
            };
            // `webSearch: auto` and friends live here too — a provider entry is
            // one that actually carries a credential.
            let Some(key) = first_key_like(sub) else {
                continue;
            };
            use sha2::Digest;
            let digest = sha2::Sha256::digest(key.as_bytes());
            let fingerprint = format!("{digest:x}")[..24].to_string();
            let last4: String = key
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            out.push(DetectedIdentity {
                provider: name,
                provider_account_id: format!("key:{fingerprint}"),
                provider_account_label: Some(format!("key ••••{last4}")),
                account_email: None,
                account_org: None,
                display_name: None,
                owner_scope: "unassigned".into(),
                detection_source: "provider_key_fingerprint".into(),
            });
        }
    }
    out
}

/// The first credential-shaped string anywhere beneath `v`.
///
/// "Credential-shaped" is structural, not a name match: long enough to be a
/// key, and free of whitespace. Field names are consulted only to PREFER an
/// obvious one, never to require it, because a config we have not seen may call
/// it anything.
fn first_key_like(v: &serde_yaml_ng::Value) -> Option<String> {
    const MIN_KEY_LEN: usize = 20;
    match v {
        serde_yaml_ng::Value::String(s) => {
            let s = s.trim();
            (s.len() >= MIN_KEY_LEN && !s.contains(char::is_whitespace)).then(|| s.to_string())
        }
        serde_yaml_ng::Value::Mapping(m) => {
            // Named-like fields first, so a key beats a neighbouring URL.
            let named = m.iter().find_map(|(k, val)| {
                let name = k.as_str()?.to_lowercase();
                (name.contains("key") || name.contains("token") || name.contains("secret"))
                    .then(|| first_key_like(val))
                    .flatten()
            });
            named.or_else(|| m.iter().find_map(|(_, val)| first_key_like(val)))
        }
        serde_yaml_ng::Value::Sequence(items) => items.iter().find_map(first_key_like),
        _ => None,
    }
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

    /// The `FileSignatures` strategy was declared in [`Strategy`] and
    /// implemented nowhere — `discover()` never consulted it, so the enum
    /// variant was a promise the code did not keep. Its job is the one thing no
    /// other strategy can do: find a tool that is in no list of ours.
    #[test]
    fn a_transcript_store_no_source_lists_is_found_by_its_shape() {
        let root = std::env::temp_dir().join(format!("modelstat-sig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mk = |rel: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"{}").unwrap();
        };
        // A store shaped like every JSONL agent's, under a name nothing knows.
        mk(".newagent/sessions/2026/a.jsonl");
        // A directory with no transcripts at all is not a tool.
        std::fs::create_dir_all(root.join(".just-config/settings")).unwrap();
        // Deep enough to be past the depth cap.
        mk(".toodeep/a/b/c/d/e/buried.jsonl");
        // A cache the skip list prunes, transcripts or not.
        mk(".cachey/node_modules/pkg/x.jsonl");

        let found = file_signature_installs(&root, Children::Hidden, &BTreeSet::new());
        let agents: Vec<&str> = found.iter().map(|i| i.agent.as_str()).collect();
        assert_eq!(
            agents,
            ["newagent"],
            "the DIRECTORY's own name, dot stripped — nothing else states one"
        );
        assert_eq!(found[0].detected_via, ["file_signatures"]);
        assert!(found[0].data_dir.as_deref().unwrap().ends_with(".newagent"));

        // A directory a known source already claims belongs to that source, not
        // to a second entry under a different name.
        let claimed = BTreeSet::from([root.join(".newagent").to_string_lossy().into_owned()]);
        assert!(file_signature_installs(&root, Children::Hidden, &claimed).is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The probe never OPENS a visible directory under a home.
    ///
    /// This is the whole of the macOS consent bug: the walk used to read every
    /// child of `$HOME`, so `~/Desktop`, `~/Documents`, `~/Downloads`,
    /// `~/Pictures` and `~/Music` were opened on every discovery pass and macOS
    /// interrupted the user with a TCC dialog for each — Photo Library, Apple
    /// Music, one per folder — every ten seconds, forever.
    ///
    /// Asserted through the RESULT rather than by counting syscalls, because the
    /// two are the same statement here: a directory only becomes an
    /// installation by being read, so a `Documents` that holds a transcript and
    /// is still not reported is a `Documents` that was never opened.
    #[test]
    fn a_homes_visible_folders_are_never_opened() {
        let root = std::env::temp_dir().join(format!("modelstat-tcc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mk = |rel: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"{}").unwrap();
        };
        // Every macOS-guarded folder, each holding a transcript-shaped file so
        // that reading it WOULD report an install. Nothing may.
        for guarded in ["Desktop", "Documents", "Downloads", "Pictures", "Music"] {
            mk(&format!("{guarded}/notes/session.jsonl"));
        }
        // The user's own folders are visible too, and just as much not ours.
        mk("Dropbox/work/a.jsonl");
        mk("projects/repo/b.jsonl");
        // An agent's real store, which must survive the rule.
        mk(".newagent/sessions/a.jsonl");

        let found = file_signature_installs(&root, Children::Hidden, &BTreeSet::new());
        let agents: Vec<&str> = found.iter().map(|i| i.agent.as_str()).collect();
        assert_eq!(
            agents,
            ["newagent"],
            "only hidden children of a home may be opened"
        );

        // The same tree under an APPLICATION-DATA root is a different question:
        // there the children are app data directories and visible names are
        // exactly what they carry.
        let any = file_signature_installs(&root, Children::Any, &BTreeSet::new());
        assert!(
            any.iter().any(|i| i.agent == "Documents"),
            "an application-data root keeps its visible children"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Apple's private stores live inside an application-data root, among
    /// ordinary app directories, and opening one raises a Contacts consent
    /// dialog. The `Children::Any` rule cannot exclude them — only the name can.
    #[test]
    fn the_guarded_stores_inside_an_app_data_root_are_never_opened() {
        let root = std::env::temp_dir().join(format!("modelstat-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mk = |rel: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"{}").unwrap();
        };
        mk("AddressBook/Sources/x.jsonl");
        mk("MobileSync/Backup/y.jsonl");
        mk("Knowledge/z.jsonl");
        mk("SomeEditor/sessions/a.jsonl");

        let found = file_signature_installs(&root, Children::Any, &BTreeSet::new());
        let agents: Vec<&str> = found.iter().map(|i| i.agent.as_str()).collect();
        assert_eq!(agents, ["SomeEditor"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The wiring, over a whole home: each root walked under its own rule.
    ///
    /// The per-root rules are asserted above; this is the statement that the
    /// PROBE applies them to the right roots, which is what actually regressed.
    /// A home's guarded folders each hold a transcript, so any of them being
    /// reported means it was opened — and an open is what raises the dialog.
    #[test]
    fn the_probe_walks_a_home_by_the_hidden_rule_and_app_data_by_its_own() {
        let home = std::env::temp_dir().join(format!("modelstat-wiring-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mk = |rel: &str| {
            let p = home.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"{}").unwrap();
        };
        for guarded in ["Desktop", "Documents", "Downloads", "Pictures", "Music"] {
            mk(&format!("{guarded}/notes/session.jsonl"));
        }
        mk("Library/Application Support/AddressBook/Sources/x.jsonl");
        // A hidden agent store, and an app-data one under a name nothing knows.
        mk(".newagent/sessions/a.jsonl");
        mk("Library/Application Support/Some Editor/sessions/b.jsonl");
        // `~/.config` is both an application-data root AND a dotdir under the
        // home. Walked as a root, it must not ALSO be reported as an agent
        // called "config".
        mk(".config/othertool/sessions/c.jsonl");

        let mut agents: Vec<String> = probe_file_signatures_in(&home)
            .into_iter()
            .map(|i| i.agent)
            .collect();
        agents.sort();
        assert_eq!(agents, ["Some Editor", "newagent", "othertool"]);

        let _ = std::fs::remove_dir_all(&home);
    }

    /// A deleted tool is not an installed one — on either platform's trash.
    #[test]
    fn a_trashed_store_is_not_reported_as_an_install() {
        let root = std::env::temp_dir().join(format!("modelstat-trash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for rel in [
            ".Trash/.oldtool/sessions/a.jsonl",
            ".local/share/Trash/files/.oldtool/b.jsonl",
        ] {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"{}").unwrap();
        }

        assert!(file_signature_installs(&root, Children::Hidden, &BTreeSet::new()).is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An injected home means EVERY home-relative form resolves inside it.
    ///
    /// `%APPDATA%` is always set on Windows, so honouring it while expanding
    /// somebody else's home sent discovery to the real roaming profile and
    /// quietly ignored the root it was handed — caught by the Windows CI lane,
    /// invisible on macOS and Linux where the variable is usually unset.
    #[test]
    fn platform_base_dirs_follow_the_home_they_are_given() {
        let root = std::env::temp_dir().join("modelstat-not-a-real-home");
        for (raw, tail) in [
            ("$APPDATA/Cursor", "AppData/Roaming/Cursor"),
            ("$LOCALAPPDATA/Cursor", "AppData/Local/Cursor"),
            ("$XDG_CONFIG_HOME/codex", ".config/codex"),
            ("$XDG_DATA_HOME/codex", ".local/share/codex"),
            ("~/.claude", ".claude"),
            ("$HOME/.claude", ".claude"),
        ] {
            // Compared as PATHS: the expansion emits the platform's separators,
            // which is the whole point of the normalisation it ends with.
            assert_eq!(
                PathBuf::from(expand_path_with_home(&root, raw)),
                root.join(tail).components().collect::<PathBuf>(),
                "{raw} escaped the home it was given"
            );
        }
    }

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

#[cfg(test)]
mod provider_key_tests {
    use super::*;

    fn write_cfg(body: &str) -> String {
        let home = std::env::temp_dir().join(format!(
            "modelstat-pikeys-{}-{}",
            std::process::id(),
            body.len()
        ));
        let dir = home.join(".pi/agent");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yml"), body).unwrap();
        home.to_string_lossy().into_owned()
    }

    /// The nesting under `providers:` was never observable, so ANY of these
    /// shapes must work. A layout we guessed wrong yields nothing, never
    /// something false.
    #[test]
    fn a_key_is_found_at_whatever_depth_it_sits() {
        for body in [
            "providers:\n  zhipu: abcdefghijklmnopqrstuvwxyz123456\n",
            "providers:\n  zhipu:\n    apiKey: abcdefghijklmnopqrstuvwxyz123456\n",
            "providers:\n  zhipu:\n    auth:\n      token: abcdefghijklmnopqrstuvwxyz123456\n",
            "providers:\n  zhipu:\n    keys:\n      - abcdefghijklmnopqrstuvwxyz123456\n",
        ] {
            let home = write_cfg(body);
            let ids = probe_provider_key_identities(&home);
            assert_eq!(ids.len(), 1, "one identity for {body:?}");
            assert_eq!(ids[0].provider, "zhipu");
            assert_eq!(
                ids[0].provider_account_label.as_deref(),
                Some("key ••••3456")
            );
            assert!(ids[0].provider_account_id.starts_with("key:"));
            std::fs::remove_dir_all(&home).ok();
        }
    }

    /// The key itself must never appear in what the probe hands back.
    #[test]
    fn the_key_never_leaves_the_probe() {
        const SECRET: &str = "zhipu-live-abcdefghijklmnopqrstuvwxyz";
        let home = write_cfg(&format!("providers:\n  zhipu:\n    apiKey: {SECRET}\n"));
        let ids = probe_provider_key_identities(&home);
        let dumped = format!("{ids:?}");
        assert!(!dumped.contains(SECRET), "the key leaked: {dumped}");
        assert!(!dumped.contains("abcdefghij"), "even a slice of it leaked");
        // Same key ⇒ same account, so spend groups correctly.
        let again = probe_provider_key_identities(&home);
        assert_eq!(ids[0].provider_account_id, again[0].provider_account_id);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn non_credential_settings_are_not_mistaken_for_accounts() {
        // The real config on the machine this was written on: a providers block
        // holding a plain setting and no credential at all.
        let home = write_cfg("providers:\n  webSearch: auto\nsymbolPreset: unicode\n");
        assert!(
            probe_provider_key_identities(&home).is_empty(),
            "a setting is not an account"
        );
        std::fs::remove_dir_all(&home).ok();

        // A URL is long but has a scheme and is not a credential field.
        let home =
            write_cfg("providers:\n  zhipu:\n    baseUrl: https://open.bigmodel.cn/api/paas\n");
        let ids = probe_provider_key_identities(&home);
        assert!(
            ids.is_empty() || ids[0].provider_account_id.starts_with("key:"),
            "a base URL must not masquerade as a key"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_missing_or_broken_config_is_silent() {
        assert!(probe_provider_key_identities("/no/such/home").is_empty());
        let home = write_cfg("this: [is not: valid yaml\n");
        assert!(probe_provider_key_identities(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn gh_hosts_yield_the_active_login_and_the_remembered_ones_never_a_token() {
        let raw = "github.com:\n    user: aramalipoor\n    oauth_token: gho_SECRET_SHOULD_NEVER_TRAVEL\n    git_protocol: https\n    users:\n        aramalipoor:\n            oauth_token: gho_SECRET\n        other-login:\n            oauth_token: gho_SECRET2\nghe.acme.com:\n    user: acme-aram\n";
        let out = dedupe_handles(handles_from_gh_hosts(raw));
        let pairs: Vec<(String, String)> = out
            .iter()
            .map(|h| (h.provider.clone(), h.handle.clone()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("github".to_string(), "aramalipoor".to_string()),
                ("github".to_string(), "other-login".to_string()),
                ("ghe.acme.com".to_string(), "acme-aram".to_string()),
            ]
        );
        let serialized = serde_json::to_string(&out).unwrap();
        assert!(
            !serialized.contains("gho_"),
            "a token never leaves the file"
        );
        assert!(out.iter().all(|h| h.detection_source == "gh_cli_hosts"));
    }

    #[test]
    fn gh_hosts_that_are_not_yaml_or_name_nobody_yield_nothing() {
        assert!(handles_from_gh_hosts("not: [valid").is_empty());
        assert!(handles_from_gh_hosts("github.com:\n    git_protocol: https\n").is_empty());
    }

    #[test]
    fn git_identity_is_the_email_lowercased_with_the_name_beside_it() {
        let out = git_identity_handles(
            Some("  Aram.Alipoor@Example.com\n".into()),
            Some("Aram Alipoor\n".into()),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].provider, "email");
        assert_eq!(out[0].handle, "aram.alipoor@example.com");
        assert_eq!(out[0].email.as_deref(), Some("aram.alipoor@example.com"));
        assert_eq!(out[0].display_name.as_deref(), Some("Aram Alipoor"));
        assert!(git_identity_handles(None, Some("Nobody".into())).is_empty());
        assert!(git_identity_handles(Some("not-an-email".into()), None).is_empty());
    }

    #[test]
    fn dedupe_handles_keeps_first_and_fills_gaps() {
        let a = DetectedHandle {
            provider: "email".into(),
            handle: "a@x.com".into(),
            display_name: None,
            email: Some("a@x.com".into()),
            detection_source: "git_config".into(),
        };
        let b = DetectedHandle {
            provider: "email".into(),
            handle: "A@x.com".into(),
            display_name: Some("A".into()),
            email: None,
            detection_source: "other".into(),
        };
        let out = dedupe_handles(vec![a, b]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].handle, "a@x.com");
        assert_eq!(out[0].display_name.as_deref(), Some("A"));
        assert_eq!(out[0].detection_source, "git_config");
    }
}
