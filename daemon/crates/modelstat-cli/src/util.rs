//! Shared CLI helpers (feature §5): the per-OS browser opener, interactive
//! stdin prompts, the `last-status.json` reader, and small URL helpers. Ported
//! from `apps/daemon/src/cli.ts` (`tryOpenBrowser`, `textPrompt`,
//! `readLocalStatus`).

use std::io::{BufRead, IsTerminal, Write};

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

/// The controlling terminal, when this process has one.
///
/// The documented install path is `curl -fsSL …/install.sh | sh`, which puts the
/// *script* on stdin — so stdin is a pipe even though the person is sitting at a
/// real terminal. Gating prompts on `stdin.is_terminal()` therefore made the
/// advertised one-liner unable to ever answer the summariser-mode consent
/// question: it always took the non-interactive branch and aborted the install.
///
/// `/dev/tty` is that person's terminal regardless of how stdin is wired, which
/// is the standard way a piped installer stays interactive. It genuinely fails
/// to open when there is no terminal at all — CI, a launchd/systemd unit, a
/// detached daemon — so the consent gate still refuses to guess in exactly the
/// cases it must.
#[cfg(unix)]
fn controlling_tty() -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()
}

/// Windows has no `/dev/tty`; prompts there stay gated on stdin.
#[cfg(not(unix))]
fn controlling_tty() -> Option<std::fs::File> {
    None
}

/// Read one line from the user after writing `prompt`. Prefers stdin when it is
/// a terminal, else falls back to the controlling terminal (see
/// [`controlling_tty`]). Returns the trimmed line, or the empty string on EOF /
/// no terminal at all. Port of the readline half of TS `textPrompt`.
pub fn prompt_line(prompt: &str) -> String {
    let trim = |line: String| line.trim_end_matches(['\n', '\r']).trim().to_string();
    if std::io::stdin().is_terminal() {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        return match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => String::new(),
            Ok(_) => trim(line),
        };
    }
    // Write the prompt to the same terminal we read from — under `curl | sh`
    // stdout is usually still the terminal, but not when the caller redirects it.
    let Some(mut tty) = controlling_tty() else {
        return String::new();
    };
    let _ = write!(tty, "{prompt}");
    let _ = tty.flush();
    let mut line = String::new();
    match std::io::BufReader::new(tty).read_line(&mut line) {
        Ok(0) | Err(_) => String::new(),
        Ok(_) => trim(line),
    }
}

/// A prompt with a default: empty input (or no terminal at all) yields
/// `default`. TS `textPrompt(prompt, def)`.
pub fn text_prompt(prompt: &str, default: &str) -> String {
    if !has_terminal() {
        return default.to_string();
    }
    let v = prompt_line(prompt);
    if v.is_empty() {
        default.to_string()
    } else {
        v
    }
}

/// True when there is an interactive terminal to prompt on — stdin itself, or
/// the controlling terminal when stdin is a pipe (`curl … | sh`). Mode prompts
/// are gated on this.
pub fn has_terminal() -> bool {
    std::io::stdin().is_terminal() || controlling_tty().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Widening the check to `/dev/tty` must never *narrow* it: anything that
    /// counted as interactive before (stdin itself a TTY) still must.
    #[test]
    fn a_tty_stdin_always_counts_as_interactive() {
        assert!(!std::io::stdin().is_terminal() || has_terminal());
    }

    /// The safety half of the `/dev/tty` fallback, asserted where it matters.
    /// A headless runner has no controlling terminal, so the summariser-mode
    /// consent gate must still take the non-interactive branch and refuse to
    /// guess — the fallback buys `curl … | sh` a prompt, never CI a silent
    /// default. Only meaningful without a terminal, hence the CI guard: run
    /// from a developer's shell this process *does* have one.
    #[test]
    fn headless_stays_non_interactive() {
        if std::env::var("CI").map(|v| v.is_empty()).unwrap_or(true) {
            return;
        }
        assert!(!has_terminal(), "CI must not look interactive");
        assert_eq!(prompt_line("ignored: "), "");
        assert_eq!(text_prompt("ignored: ", "fallback"), "fallback");
    }
}
