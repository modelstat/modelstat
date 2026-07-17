//! Shared CLI helpers (feature §5): the per-OS browser opener, interactive
//! stdin prompts, the `last-status.json` reader, and small URL helpers. Ported
//! from `apps/daemon/src/cli.ts` (`tryOpenBrowser`, `textPrompt`,
//! `readLocalStatus`).

use std::io::{IsTerminal, Write};

use modelstat_ingest::{home_path, Config};
use serde_json::Value;

/// Open `url` in the default browser (best-effort, detached — never blocks).
/// macOS `open`, Windows `cmd /c start "" <url>`, else `xdg-open`. Returns
/// `false` only when the spawn itself fails. Port of TS `tryOpenBrowser`.
pub fn open_browser(url: &str) -> bool {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", "start", "", url]);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// The local heartbeat mirror `~/.modelstat/last-status.json`, parsed into an
/// order-preserving `Value` (serde_json `preserve_order` is on workspace-wide),
/// or `None` when absent/unreadable. Re-serialized (not byte-copied) so `status`
/// emits compact canonical JSON — TS `readLocalStatus` parity.
pub fn read_local_status() -> Option<Value> {
    let raw = std::fs::read_to_string(home_path("last-status.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

/// `<api>/dashboard` with a single trailing slash stripped from the base.
pub fn dashboard_url(config: &Config) -> String {
    format!("{}/dashboard", config.api_url().trim_end_matches('/'))
}

/// `<api>` with a single trailing slash stripped.
pub fn api_base(config: &Config) -> String {
    config.api_url().trim_end_matches('/').to_string()
}

/// Read one line from stdin after writing `prompt` to stdout. Returns the
/// trimmed line, or the empty string on EOF / a closed (non-interactive) stdin.
/// Port of the readline half of TS `textPrompt`.
pub fn prompt_line(prompt: &str) -> String {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => String::new(),
        Ok(_) => line.trim_end_matches(['\n', '\r']).trim().to_string(),
    }
}

/// A prompt with a default: empty input (or a non-interactive stdin) yields
/// `default`. TS `textPrompt(prompt, def)`.
pub fn text_prompt(prompt: &str, default: &str) -> String {
    if !std::io::stdin().is_terminal() {
        return default.to_string();
    }
    let v = prompt_line(prompt);
    if v.is_empty() {
        default.to_string()
    } else {
        v
    }
}

/// True when stdin is an interactive TTY (mode prompts are gated on it).
pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}
