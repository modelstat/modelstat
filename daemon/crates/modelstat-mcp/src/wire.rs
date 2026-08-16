//! `modelstat mcp wire [--heal]` (§12) — drop the modelstat MCP server entry into
//! every local AI tool we can find, so the user can ask about their spend from
//! inside them. A port of `packages/mcp/src/wire.ts`.
//!
//! §12 CHANGE from the TS: the entry is now `{command: "<abs path to modelstat>",
//! args: ["mcp"]}` — no `npx`/Node. §12 IMPROVEMENT: a malformed target config is
//! backed up (`<file>.bak`) before we rebuild it, rather than silently discarded.
//!
//! Idempotent + non-destructive: for each tool we set ONLY the single `modelstat`
//! server entry and never touch sibling servers or other settings. A tool is
//! "detected" when its config dir exists; undetected tools are skipped. Always
//! exits 0.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Map, Value};

/// Platform, injected so the path resolution is testable off the host OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plat {
    Macos,
    Linux,
    Windows,
}

impl Plat {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Plat::Macos,
            "windows" => Plat::Windows,
            _ => Plat::Linux,
        }
    }
}

/// The outcome of wiring one tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireStatus {
    /// We wrote (or updated) the entry.
    Configured,
    /// Ours was already present + identical.
    Already,
    /// The tool isn't installed (its config dir is absent).
    Absent,
    /// Detected, but we couldn't write the config.
    Skipped,
}

impl WireStatus {
    /// The report mark (`+ · - !`).
    pub fn mark(&self) -> char {
        match self {
            WireStatus::Configured => '+',
            WireStatus::Already => '·',
            WireStatus::Absent => '-',
            WireStatus::Skipped => '!',
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            WireStatus::Configured => "configured",
            WireStatus::Already => "already configured",
            WireStatus::Absent => "not detected, skipped",
            WireStatus::Skipped => "could not write, skipped",
        }
    }
}

/// Server-entry shape: most tools take a plain `{command, args}`; Zed wraps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Command,
    Zed,
}

/// One JSON/JSONC config target.
#[derive(Debug, Clone)]
pub struct JsonTarget {
    pub name: &'static str,
    /// Config file we merge into.
    pub file: PathBuf,
    /// Top-level key holding the server map (`mcpServers` / `servers` / `context_servers`).
    pub key: &'static str,
    /// Dir whose existence means the tool is installed.
    pub detect: PathBuf,
    pub shape: Shape,
}

/// Per-OS application-support base dir for a GUI app's config.
fn app_support(home: &Path, plat: Plat, app: &str) -> PathBuf {
    match plat {
        Plat::Windows => std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join(app),
        Plat::Macos => home.join("Library/Application Support").join(app),
        Plat::Linux => home.join(".config").join(app),
    }
}

/// The JSON config targets for a home + platform (§12: Zed lives under
/// `%APPDATA%\Zed` on Windows, else `~/.config/zed`).
pub fn json_targets(home: &Path, plat: Plat) -> Vec<JsonTarget> {
    let claude = app_support(home, plat, "Claude");
    let code = app_support(home, plat, "Code");
    let code_insiders = app_support(home, plat, "Code - Insiders");
    let vscodium = app_support(home, plat, "VSCodium");
    let zed_dir = match plat {
        Plat::Windows => app_support(home, plat, "Zed"),
        _ => home.join(".config/zed"),
    };
    vec![
        JsonTarget {
            name: "Claude Desktop",
            file: claude.join("claude_desktop_config.json"),
            key: "mcpServers",
            detect: claude,
            shape: Shape::Command,
        },
        JsonTarget {
            name: "Cursor",
            file: home.join(".cursor/mcp.json"),
            key: "mcpServers",
            detect: home.join(".cursor"),
            shape: Shape::Command,
        },
        JsonTarget {
            name: "VS Code",
            file: code.join("User/mcp.json"),
            key: "servers",
            detect: code,
            shape: Shape::Command,
        },
        JsonTarget {
            name: "VS Code Insiders",
            file: code_insiders.join("User/mcp.json"),
            key: "servers",
            detect: code_insiders,
            shape: Shape::Command,
        },
        JsonTarget {
            name: "VSCodium",
            file: vscodium.join("User/mcp.json"),
            key: "servers",
            detect: vscodium,
            shape: Shape::Command,
        },
        JsonTarget {
            name: "Windsurf",
            file: home.join(".codeium/windsurf/mcp_config.json"),
            key: "mcpServers",
            detect: home.join(".codeium/windsurf"),
            shape: Shape::Command,
        },
        JsonTarget {
            name: "Gemini CLI",
            file: home.join(".gemini/settings.json"),
            key: "mcpServers",
            detect: home.join(".gemini"),
            shape: Shape::Command,
        },
        JsonTarget {
            name: "Zed",
            file: zed_dir.join("settings.json"),
            key: "context_servers",
            detect: zed_dir,
            shape: Shape::Zed,
        },
    ]
}

/// The desired `modelstat` server entry for a shape (§12: absolute binary + `mcp`).
fn server_entry(exe: &str, shape: Shape) -> Value {
    match shape {
        Shape::Command => json!({ "command": exe, "args": ["mcp"] }),
        Shape::Zed => json!({ "source": "custom", "command": exe, "args": ["mcp"], "env": {} }),
    }
}

/// Merge the modelstat server into one JSON config. Only the `modelstat` entry is
/// touched. A malformed existing file is backed up (`<file>.bak`) before rebuild.
pub fn wire_json_target(t: &JsonTarget, exe: &str) -> WireStatus {
    if !t.detect.exists() {
        return WireStatus::Absent;
    }
    let mut cfg: Map<String, Value> = Map::new();
    if t.file.exists() {
        match std::fs::read_to_string(&t.file) {
            Ok(raw) if !raw.trim().is_empty() => match serde_json::from_str::<Value>(&raw) {
                Ok(Value::Object(m)) => cfg = m,
                _ => {
                    // Malformed / non-object → back up the original, then rebuild.
                    let bak = PathBuf::from(format!("{}.bak", t.file.display()));
                    let _ = std::fs::rename(&t.file, bak);
                }
            },
            _ => {}
        }
    }
    let mut bag: Map<String, Value> = match cfg.get(t.key) {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let desired = server_entry(exe, t.shape);
    if bag.get("modelstat") == Some(&desired) {
        return WireStatus::Already;
    }
    bag.insert("modelstat".to_string(), desired);
    cfg.insert(t.key.to_string(), Value::Object(bag));

    if let Some(dir) = t.file.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return WireStatus::Skipped;
        }
    }
    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(cfg)).unwrap_or_default()
    );
    if std::fs::write(&t.file, body).is_ok() {
        WireStatus::Configured
    } else {
        WireStatus::Skipped
    }
}

/// Codex uses TOML; write the `[mcp_servers.modelstat]` table, REPLACING any
/// existing one. §MIGRATION: the old npm daemon wrote this table as
/// `command = "npx"` (`@modelstat/mcp`); a plain "append if absent" would leave
/// that stale entry forever, so we strip any existing `[mcp_servers.modelstat]`
/// table and append ours. A no-op (`Already`) when the existing table is already
/// ours.
pub fn wire_codex(home: &Path, exe: &str) -> WireStatus {
    let dir = home.join(".codex");
    if !dir.exists() {
        return WireStatus::Absent;
    }
    let file = dir.join("config.toml");
    let toml = std::fs::read_to_string(&file).unwrap_or_default();
    // TOML string escaping for the (possibly space-bearing) path.
    let esc = exe.replace('\\', "\\\\").replace('"', "\\\"");
    let block = format!("[mcp_servers.modelstat]\ncommand = \"{esc}\"\nargs = [\"mcp\"]\n");

    // Rebuild the file with any existing `[mcp_servers.modelstat]` table dropped —
    // a table runs from its header to the next `[...]` header (or EOF). Track
    // whether the dropped table was already ours so an unchanged file is a no-op.
    let mut kept: Vec<&str> = Vec::new();
    let mut in_table = false;
    let mut had_table = false;
    let mut existing_is_ours = false;
    let ours_command = format!("command = \"{esc}\"");
    for line in toml.lines() {
        let t = line.trim();
        if t == "[mcp_servers.modelstat]" {
            in_table = true;
            had_table = true;
            continue; // drop the header
        }
        if in_table {
            if t.starts_with('[') {
                in_table = false; // a new table begins — keep it (fall through)
            } else {
                if t == ours_command {
                    existing_is_ours = true;
                }
                continue; // drop the old table body
            }
        }
        kept.push(line);
    }
    if had_table && existing_is_ours {
        return WireStatus::Already;
    }
    let base = kept.join("\n");
    let base = base.trim();
    let next = if base.is_empty() {
        block
    } else {
        format!("{base}\n\n{block}")
    };
    if std::fs::write(&file, next).is_ok() {
        WireStatus::Configured
    } else {
        WireStatus::Skipped
    }
}

/// Claude Code ships a CLI; register a user-scoped `modelstat` server pointing at
/// the native binary. `claude.cmd` is tried on Windows. `exe` is the absolute
/// modelstat path.
///
/// §MIGRATION: the old npm daemon registered this same name as
/// `npx -y @modelstat/mcp`, and `claude mcp add` REFUSES to overwrite an existing
/// server (its dominant failure is "already exists in this scope"). A plain add
/// would therefore leave a migrated user invoking the dead npm connector forever —
/// so when an entry already exists and isn't ours, we remove it and add fresh.
pub fn wire_claude_code(exe: &str) -> WireStatus {
    let claude = if cfg!(windows) {
        "claude.cmd"
    } else {
        "claude"
    };
    if Command::new(claude)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        return WireStatus::Absent; // CLI not on PATH
    }
    let add = || {
        Command::new(claude)
            .args(["mcp", "add", "modelstat", "-s", "user", "--", exe, "mcp"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    // A plain add both installs a fresh entry AND (by failing) tells us one already
    // exists.
    if add() {
        return WireStatus::Configured;
    }
    // An entry exists. If it's already ours (`claude mcp get` names the native exe)
    // we're done; otherwise it's the old `npx @modelstat/mcp` — remove it and add
    // ours. (`get` unavailable/unrecognised → treat as stale and replace.)
    let ours = Command::new(claude)
        .args(["mcp", "get", "modelstat"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(exe))
        .unwrap_or(false);
    if ours {
        return WireStatus::Already;
    }
    let _ = Command::new(claude)
        .args(["mcp", "remove", "modelstat", "-s", "user"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if add() {
        WireStatus::Configured
    } else {
        WireStatus::Skipped
    }
}

/// One tool's wire result.
#[derive(Debug, Clone)]
pub struct WireResult {
    pub name: &'static str,
    pub status: WireStatus,
}

/// Wire every detected tool (user-initiated `wire` — (re)configures everything).
/// Best-effort; the caller always exits 0. `include_claude_code=false` in tests
/// skips the CLI shell-out.
pub fn run_wire(home: &Path, plat: Plat, exe: &str, include_claude_code: bool) -> Vec<WireResult> {
    let mut results = Vec::new();
    if include_claude_code {
        results.push(WireResult {
            name: "Claude Code",
            status: wire_claude_code(exe),
        });
    }
    for t in json_targets(home, plat) {
        results.push(WireResult {
            name: t.name,
            status: wire_json_target(&t, exe),
        });
    }
    results.push(WireResult {
        name: "Codex",
        status: wire_codex(home, exe),
    });
    results
}

/// Where the heal state lives (`~/.modelstat/mcp-wired.json`).
pub fn wired_state_path() -> PathBuf {
    modelstat_ingest::home_path("mcp-wired.json")
}

fn read_wired_set(path: &Path) -> std::collections::BTreeSet<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("wired").and_then(Value::as_array).cloned())
        .map(|arr| {
            arr.into_iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn write_wired_set(path: &Path, names: &std::collections::BTreeSet<String>) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = PathBuf::from(format!("{}.tmp-{}", path.display(), std::process::id()));
    let body = format!("{}\n", json!({ "wired": names }));
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The outcome of a self-heal run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealResult {
    /// Clients newly configured this run.
    pub configured: Vec<String>,
    /// How many clients were already tracked (skipped — never re-added).
    pub tracked: usize,
}

/// Self-heal wiring (`wire --heal`, run by the daemon on startup). Configures ONLY
/// clients not already tracked (so a client the user removed is left alone), then
/// records every currently-installed client so a later run won't touch them again.
/// One named wiring attempt, deferred so `heal_wire` can skip the ones the
/// state file says already ran.
type WireRunner<'a> = Box<dyn Fn() -> WireStatus + 'a>;

/// Best-effort. `state_path` + `include_claude_code` injected for tests.
pub fn heal_wire(
    home: &Path,
    plat: Plat,
    exe: &str,
    include_claude_code: bool,
    state_path: &Path,
) -> HealResult {
    let recorded = read_wired_set(state_path);
    let mut runners: Vec<(&'static str, WireRunner<'_>)> = Vec::new();
    if include_claude_code {
        runners.push(("Claude Code", Box::new(move || wire_claude_code(exe))));
    }
    for t in json_targets(home, plat) {
        runners.push((t.name, Box::new(move || wire_json_target(&t, exe))));
    }
    runners.push(("Codex", Box::new(move || wire_codex(home, exe))));

    let mut configured = Vec::new();
    let mut next = recorded.clone();
    for (name, run) in runners {
        if recorded.contains(name) {
            continue; // wired once already — never re-add (respect a user removal)
        }
        let status = run();
        if status == WireStatus::Configured {
            configured.push(name.to_string());
        }
        // Mark every DETECTED (installed) client as seen; an absent one stays
        // un-recorded so a later install still gets wired.
        if matches!(status, WireStatus::Configured | WireStatus::Already) {
            next.insert(name.to_string());
        }
    }
    if next.len() != recorded.len() {
        write_wired_set(state_path, &next);
    }
    HealResult {
        configured,
        tracked: recorded.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/Users/x/.modelstat/bin/modelstat";

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("modelstat-wire-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn read(p: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn absent_tool_is_skipped() {
        let home = tmp("absent");
        let targets = json_targets(&home, Plat::Macos);
        // Nothing exists → every target reports absent.
        for t in &targets {
            assert_eq!(wire_json_target(t, EXE), WireStatus::Absent);
        }
    }

    #[test]
    fn wires_cursor_with_the_absolute_binary_and_mcp_arg() {
        let home = tmp("cursor");
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        let target = json_targets(&home, Plat::Macos)
            .into_iter()
            .find(|t| t.name == "Cursor")
            .unwrap();
        assert_eq!(wire_json_target(&target, EXE), WireStatus::Configured);
        let v = read(&home.join(".cursor/mcp.json"));
        assert_eq!(v["mcpServers"]["modelstat"]["command"], EXE);
        assert_eq!(v["mcpServers"]["modelstat"]["args"][0], "mcp");
        // Re-wire is a no-op.
        assert_eq!(wire_json_target(&target, EXE), WireStatus::Already);
    }

    #[test]
    fn sibling_servers_are_preserved() {
        let home = tmp("sibling");
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        std::fs::write(
            home.join(".cursor/mcp.json"),
            r#"{"mcpServers":{"other":{"command":"x"}},"someSetting":true}"#,
        )
        .unwrap();
        let target = json_targets(&home, Plat::Macos)
            .into_iter()
            .find(|t| t.name == "Cursor")
            .unwrap();
        assert_eq!(wire_json_target(&target, EXE), WireStatus::Configured);
        let v = read(&home.join(".cursor/mcp.json"));
        assert_eq!(v["mcpServers"]["other"]["command"], "x"); // untouched
        assert_eq!(v["someSetting"], true); // untouched
        assert_eq!(v["mcpServers"]["modelstat"]["command"], EXE);
    }

    #[test]
    fn zed_uses_the_custom_source_shape() {
        let home = tmp("zed");
        std::fs::create_dir_all(home.join(".config/zed")).unwrap();
        let target = json_targets(&home, Plat::Linux)
            .into_iter()
            .find(|t| t.name == "Zed")
            .unwrap();
        assert_eq!(wire_json_target(&target, EXE), WireStatus::Configured);
        let v = read(&home.join(".config/zed/settings.json"));
        assert_eq!(v["context_servers"]["modelstat"]["source"], "custom");
        assert_eq!(v["context_servers"]["modelstat"]["command"], EXE);
    }

    #[test]
    fn malformed_config_is_backed_up_then_rebuilt() {
        let home = tmp("malformed");
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        let file = home.join(".cursor/mcp.json");
        std::fs::write(&file, "{ not valid json").unwrap();
        let target = json_targets(&home, Plat::Macos)
            .into_iter()
            .find(|t| t.name == "Cursor")
            .unwrap();
        assert_eq!(wire_json_target(&target, EXE), WireStatus::Configured);
        // The corrupt original was preserved as .bak (§12 improvement).
        let bak = PathBuf::from(format!("{}.bak", file.display()));
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "{ not valid json");
        assert_eq!(read(&file)["mcpServers"]["modelstat"]["command"], EXE);
    }

    #[test]
    fn codex_toml_table_is_appended_once() {
        let home = tmp("codex");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/config.toml"), "model = \"gpt\"\n").unwrap();
        assert_eq!(wire_codex(&home, EXE), WireStatus::Configured);
        let toml = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(toml.contains("model = \"gpt\""), "{toml}"); // preserved
        assert!(toml.contains("[mcp_servers.modelstat]"), "{toml}");
        assert!(toml.contains("args = [\"mcp\"]"), "{toml}");
        // Re-run is a no-op.
        assert_eq!(wire_codex(&home, EXE), WireStatus::Already);
    }

    #[test]
    fn codex_toml_replaces_the_stale_npx_table() {
        // §MIGRATION: an existing `[mcp_servers.modelstat]` from the old npm daemon
        // (`command = "npx"`) must be REPLACED by the native entry — not left behind
        // — while sibling tables and preamble survive.
        let home = tmp("codex-migrate");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "model = \"gpt\"\n\n[mcp_servers.modelstat]\ncommand = \"npx\"\nargs = [\"-y\", \"@modelstat/mcp\"]\n\n[mcp_servers.other]\ncommand = \"foo\"\n",
        )
        .unwrap();
        assert_eq!(wire_codex(&home, EXE), WireStatus::Configured);
        let toml = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(!toml.contains("npx"), "stale npx command survived:\n{toml}");
        assert!(
            !toml.contains("@modelstat/mcp"),
            "stale npx args survived:\n{toml}"
        );
        assert!(toml.contains(&format!("command = \"{EXE}\"")), "{toml}");
        assert_eq!(
            toml.matches("[mcp_servers.modelstat]").count(),
            1,
            "duplicate table:\n{toml}"
        );
        assert!(
            toml.contains("[mcp_servers.other]"),
            "sibling dropped:\n{toml}"
        );
        assert!(
            toml.contains("model = \"gpt\""),
            "preamble dropped:\n{toml}"
        );
        // Idempotent now that the table is ours.
        assert_eq!(wire_codex(&home, EXE), WireStatus::Already);
    }

    #[test]
    fn windows_paths_use_appdata_and_zed_moves() {
        let home = PathBuf::from("C:/Users/dev");
        let targets = json_targets(&home, Plat::Windows);
        let zed = targets.iter().find(|t| t.name == "Zed").unwrap();
        // On Windows, Zed lives under the app-support base, not ~/.config/zed.
        assert!(!zed.detect.to_string_lossy().contains(".config"));
        assert!(zed.detect.to_string_lossy().ends_with("Zed"));
    }

    #[test]
    fn heal_wires_only_untracked_clients_and_records_them() {
        let home = tmp("heal");
        // Cursor + Gemini installed.
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        let state = home.join("mcp-wired.json");

        // First heal: both get configured + recorded.
        let r1 = heal_wire(&home, Plat::Macos, EXE, false, &state);
        assert!(r1.configured.contains(&"Cursor".to_string()));
        assert!(r1.configured.contains(&"Gemini CLI".to_string()));
        assert_eq!(r1.tracked, 0);

        // User removes our Cursor entry; a second heal must NOT re-add it
        // (Cursor is tracked), and configures nothing new.
        std::fs::write(home.join(".cursor/mcp.json"), "{}").unwrap();
        let r2 = heal_wire(&home, Plat::Macos, EXE, false, &state);
        assert!(r2.configured.is_empty(), "{:?}", r2.configured);
        assert!(r2.tracked >= 2);
        // Cursor's config stays as the user left it (no modelstat re-added).
        assert!(read(&home.join(".cursor/mcp.json"))
            .get("mcpServers")
            .is_none());
    }
}
