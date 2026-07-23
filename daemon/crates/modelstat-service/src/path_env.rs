//! PATH wiring (§3.3) — make `modelstat` runnable by name after an install.
//!
//! `_setup-runtime` stages the binaries into `~/.modelstat/bin`, a directory no
//! shell has on its PATH. Staging alone therefore leaves every command the
//! onboarding banner prints (`modelstat status`, `modelstat mode`, …) as
//! "command not found". This module is the missing half of staging.
//!
//! One shape per scope (§3.3):
//!
//! - **user** — write `~/.modelstat/env`, a tiny POSIX snippet that PREPENDS the
//!   bin dir, and source it from the login shell's startup file. Prepend, never
//!   append, so a stale `modelstat` some other package manager left on PATH can
//!   not shadow the binary we just staged. fish can't read that snippet, so it
//!   gets an equivalent drop-in under `conf.d/` — which fish auto-sources, so on
//!   fish no user-owned file is touched at all.
//! - **system** — root already owns a directory that is on everyone's PATH, so
//!   symlink `/usr/local/bin/<binary>` at the staged binary instead of editing
//!   any individual user's dotfiles.
//!
//! Windows has no dotfiles to edit: the user (or machine) `Path` environment
//! variable is the PATH, and `[Environment]::SetEnvironmentVariable` is the one
//! API that writes it without `setx`'s 1024-character truncation and that
//! broadcasts the change to the shell. We call it exactly like the rest of this
//! crate calls `schtasks`/`launchctl` — as a subprocess.
//!
//! The rendering half is pure and host-independent (unit-tested for every shell
//! on every OS, so a Windows runner can check the macOS output); the writing
//! half is the thin [`ensure_on_path`] / [`remove_from_path`] pair.

use std::io;
use std::path::{Path, PathBuf};

use modelstat_ingest::home_path;

use crate::spec::Scope;

/// The comment we stamp above everything we write. It is how a re-install finds
/// its own block to replace, and how `uninstall` finds it to delete — so it must
/// never change once shipped.
pub const MARKER: &str = "# modelstat — puts the modelstat CLI on your PATH";

/// The OS whose conventions we render for. Mirrors `modelstat_mcp::wire::Plat`;
/// passed in explicitly so the pure functions are testable from any host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Macos,
    Linux,
    Windows,
}

impl Os {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Os::Macos,
            "windows" => Os::Windows,
            _ => Os::Linux,
        }
    }
}

/// The user's login shell, as far as `$SHELL` tells us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
    /// `$SHELL` is unset or something we don't special-case (sh, dash, ksh, …) —
    /// `~/.profile` is the POSIX-portable answer.
    Posix,
}

impl Shell {
    /// Classify a `$SHELL` value (`/bin/zsh`, `/usr/local/bin/fish`, …).
    pub fn from_shell_var(shell: Option<&str>) -> Self {
        let base = shell
            .unwrap_or("")
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .trim();
        match base {
            "zsh" => Shell::Zsh,
            "bash" => Shell::Bash,
            "fish" => Shell::Fish,
            _ => Shell::Posix,
        }
    }

    /// Read it from the environment.
    pub fn current() -> Self {
        Shell::from_shell_var(std::env::var("SHELL").ok().as_deref())
    }
}

/// The startup file we wire for `shell`, under `home`.
///
/// The bash split is the whole reason this takes an OS: a macOS terminal starts
/// bash as a **login** shell, which reads `.bash_profile` and never `.bashrc`;
/// a Linux terminal starts a non-login interactive shell, which reads `.bashrc`
/// and never `.bash_profile`.
pub fn rc_path(shell: Shell, os: Os, home: &Path) -> PathBuf {
    match shell {
        Shell::Zsh => home.join(".zshrc"),
        Shell::Bash => match os {
            Os::Macos => home.join(".bash_profile"),
            _ => home.join(".bashrc"),
        },
        Shell::Fish => home
            .join(".config")
            .join("fish")
            .join("conf.d")
            .join("modelstat.fish"),
        Shell::Posix => home.join(".profile"),
    }
}

/// Every startup file we might have written across shells — what `uninstall`
/// sweeps, so switching shells after installing never strands our block.
pub fn all_rc_paths(os: Os, home: &Path) -> Vec<PathBuf> {
    [Shell::Zsh, Shell::Bash, Shell::Fish, Shell::Posix]
        .iter()
        .map(|s| rc_path(*s, os, home))
        .collect()
}

/// `~/.modelstat/env` — the POSIX snippet the startup file sources.
pub fn env_file() -> PathBuf {
    home_path("env")
}

/// Body of `~/.modelstat/env`. Prepends `bin_dir`, and is a no-op when it is
/// already there, so sourcing it twice can't grow PATH.
pub fn env_file_body(bin_dir: &Path) -> String {
    let dir = bin_dir.display();
    format!(
        "{MARKER}\n\
         # Written by the modelstat installer; sourced from your shell startup\n\
         # file. `modelstat uninstall` removes both.\n\
         case \":${{PATH}}:\" in\n\
         \x20 *\":{dir}:\"*) ;;\n\
         \x20 *) PATH=\"{dir}:${{PATH}}\"; export PATH ;;\n\
         esac\n"
    )
}

/// The two lines we append to a POSIX startup file.
pub fn rc_block(env_file: &Path) -> String {
    format!("{MARKER}\n. \"{}\"\n", env_file.display())
}

/// Body of the fish drop-in. fish auto-sources `conf.d/*.fish`, so this file is
/// self-contained — there is no fish equivalent of the `env` + `source` split.
pub fn fish_body(bin_dir: &Path) -> String {
    let dir = bin_dir.display();
    format!(
        "{MARKER}\n\
         # Written by the modelstat installer; fish auto-sources conf.d/.\n\
         # `modelstat uninstall` deletes this file.\n\
         if not contains -- \"{dir}\" $PATH\n\
         \x20   set -gx PATH \"{dir}\" $PATH\n\
         end\n"
    )
}

/// Add `block` to a startup file's `contents`, replacing any block we wrote
/// before (so a re-install after `MODELSTAT_HOME` moved re-points it instead of
/// stacking a second copy). `None` = already exactly right, nothing to write.
pub fn apply_block(contents: &str, block: &str) -> Option<String> {
    if contents.contains(block) {
        return None;
    }
    let mut out = strip_block(contents).unwrap_or_else(|| contents.to_string());
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(block);
    Some(out)
}

/// Remove our block from a startup file's `contents`. `None` = it wasn't there.
///
/// Anchored on [`MARKER`]: we drop the marker line plus the `.`/`source` line
/// that follows it, and nothing else — a user's own lines are never touched.
pub fn strip_block(contents: &str) -> Option<String> {
    if !contents.contains(MARKER) {
        return None;
    }
    let mut out = String::with_capacity(contents.len());
    let mut after_marker = false;
    for line in contents.lines() {
        if line.trim() == MARKER {
            after_marker = true;
            continue;
        }
        if after_marker {
            after_marker = false;
            let t = line.trim();
            if t.starts_with(". \"") || t.starts_with("source \"") {
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    let trimmed = out.trim_end_matches('\n');
    Some(if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    })
}

/// The file a user can `source` to get the bin dir into the shell they are
/// standing in. `None` where sourcing isn't a thing: fish (its drop-in is fish
/// syntax, and only fish reads it) and Windows (the variable is the PATH) both
/// need a fresh shell instead.
pub fn source_hint() -> Option<PathBuf> {
    if Os::current() == Os::Windows || Shell::current() == Shell::Fish {
        return None;
    }
    Some(env_file())
}

/// Is `dir` on the PATH this process inherited?
pub fn on_path(dir: &Path) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|p| p == dir)
}

/// What [`ensure_on_path`] did — printed by the installer, and read by the
/// onboarding banner to decide whether the user needs a fresh shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathWiring {
    /// Files we created or updated.
    pub files: Vec<PathBuf>,
    /// The snippet to `source` for an immediate effect in the current shell —
    /// `None` when there is nothing to source (fish, system scope, Windows).
    pub env_file: Option<PathBuf>,
    /// True when this process already sees the bin dir on PATH, i.e. the user
    /// needs no new shell at all.
    pub already_active: bool,
}

/// Put `bin_dir` on the user's PATH for `scope`. Idempotent: running it again
/// rewrites nothing when the wiring is already exactly right.
pub fn ensure_on_path(bin_dir: &Path, scope: Scope) -> io::Result<PathWiring> {
    let already_active = on_path(bin_dir);
    let os = Os::current();

    if os == Os::Windows {
        let files = windows_set_path(bin_dir, scope)?;
        return Ok(PathWiring {
            files,
            env_file: None,
            already_active,
        });
    }
    if scope == Scope::System {
        let files = link_into_system_bin(bin_dir)?;
        // The links land in a directory that is already on every PATH.
        return Ok(PathWiring {
            files,
            env_file: None,
            already_active: true,
        });
    }

    let home = os_home();
    let shell = Shell::current();
    let rc = rc_path(shell, os, &home);

    if shell == Shell::Fish {
        if let Some(parent) = rc.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = fish_body(bin_dir);
        if read_or_empty(&rc) != body {
            std::fs::write(&rc, body)?;
        }
        return Ok(PathWiring {
            files: vec![rc],
            env_file: None,
            already_active,
        });
    }

    let env = env_file();
    if let Some(parent) = env.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = env_file_body(bin_dir);
    let mut files = Vec::new();
    if read_or_empty(&env) != body {
        std::fs::write(&env, body)?;
        files.push(env.clone());
    }
    if let Some(updated) = apply_block(&read_or_empty(&rc), &rc_block(&env)) {
        std::fs::write(&rc, updated)?;
        files.push(rc);
    }
    Ok(PathWiring {
        files,
        env_file: Some(env),
        already_active,
    })
}

/// Undo [`ensure_on_path`] — called by `uninstall`. Returns what it removed.
/// Sweeps every shell's startup file, not just the current `$SHELL`, so
/// switching shells between install and uninstall leaves nothing behind.
pub fn remove_from_path(bin_dir: &Path, scope: Scope) -> io::Result<Vec<PathBuf>> {
    let os = Os::current();
    if os == Os::Windows {
        return windows_unset_path(bin_dir, scope);
    }

    let mut removed = Vec::new();
    if scope == Scope::System {
        for name in BINARIES {
            let link = Path::new(SYSTEM_BIN_DIR).join(name);
            if std::fs::read_link(&link)
                .map(|t| t == bin_dir.join(name))
                .unwrap_or(false)
            {
                std::fs::remove_file(&link)?;
                removed.push(link);
            }
        }
        return Ok(removed);
    }

    let env = env_file();
    if env.exists() {
        std::fs::remove_file(&env)?;
        removed.push(env);
    }
    for rc in all_rc_paths(os, &os_home()) {
        if !rc.exists() {
            continue;
        }
        // The fish drop-in is entirely ours — delete the file, don't edit it.
        if rc
            .file_name()
            .map(|n| n == "modelstat.fish")
            .unwrap_or(false)
        {
            if read_or_empty(&rc).contains(MARKER) {
                std::fs::remove_file(&rc)?;
                removed.push(rc);
            }
            continue;
        }
        if let Some(stripped) = strip_block(&read_or_empty(&rc)) {
            std::fs::write(&rc, stripped)?;
            removed.push(rc);
        }
    }
    Ok(removed)
}

/// The binaries `_setup-runtime` stages, and therefore the ones a system-scope
/// install exposes on PATH.
const BINARIES: [&str; 2] = ["modelstat", "modelstat-summarizer"];

/// The system-scope PATH directory (§3.3) — on every user's PATH already.
const SYSTEM_BIN_DIR: &str = "/usr/local/bin";

/// System scope: symlink the staged binaries into `/usr/local/bin`. Replaces an
/// existing link so an upgrade re-points it; refuses to clobber a real file that
/// isn't ours.
fn link_into_system_bin(bin_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    std::fs::create_dir_all(SYSTEM_BIN_DIR)?;
    for name in BINARIES {
        let target = bin_dir.join(name);
        if !target.exists() {
            continue; // engine may be absent on a collector-only archive
        }
        let link = Path::new(SYSTEM_BIN_DIR).join(name);
        match std::fs::symlink_metadata(&link) {
            Ok(meta) if meta.file_type().is_symlink() => std::fs::remove_file(&link)?,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} exists and is not a symlink — refusing to replace it",
                        link.display()
                    ),
                ))
            }
            Err(_) => {}
        }
        make_symlink(&target, &link)?;
        written.push(link);
    }
    Ok(written)
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Unreachable by construction — [`ensure_on_path`] sends Windows down the
/// environment-variable path before it gets here — but it fails loudly rather
/// than pretending to have written a link.
#[cfg(not(unix))]
fn make_symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::other(
        "system-scope PATH symlinks are POSIX-only; Windows uses the Path variable",
    ))
}

/// Windows: prepend `bin_dir` to the persisted `Path` variable for this scope.
///
/// Goes through PowerShell's `[Environment]::SetEnvironmentVariable` on purpose:
/// `setx` silently truncates a `Path` longer than 1024 characters (it would
/// destroy the user's PATH), and a raw registry write would not broadcast
/// `WM_SETTINGCHANGE`, so no already-running shell would ever see it.
fn windows_set_path(bin_dir: &Path, scope: Scope) -> io::Result<Vec<PathBuf>> {
    let target = win_target(scope);
    let dir = ps_quote(&bin_dir.display().to_string());
    // Filter first, then prepend: this both de-duplicates a previous install and
    // guarantees ours wins over any stale `modelstat` elsewhere on PATH.
    let script = format!(
        "$cur = [Environment]::GetEnvironmentVariable('Path', '{target}'); \
         if ($null -eq $cur) {{ $cur = '' }} \
         $parts = @($cur -split ';' | Where-Object {{ $_ -ne '' -and $_ -ne {dir} }}); \
         [Environment]::SetEnvironmentVariable('Path', ((@({dir}) + $parts) -join ';'), '{target}')"
    );
    run_powershell(&script)?;
    Ok(Vec::new())
}

/// Windows: drop `bin_dir` back out of the persisted `Path` variable.
fn windows_unset_path(bin_dir: &Path, scope: Scope) -> io::Result<Vec<PathBuf>> {
    let target = win_target(scope);
    let dir = ps_quote(&bin_dir.display().to_string());
    let script = format!(
        "$cur = [Environment]::GetEnvironmentVariable('Path', '{target}'); \
         if ($null -eq $cur) {{ $cur = '' }} \
         $parts = @($cur -split ';' | Where-Object {{ $_ -ne '' -and $_ -ne {dir} }}); \
         [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), '{target}')"
    );
    run_powershell(&script)?;
    Ok(Vec::new())
}

fn win_target(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "User",
        Scope::System => "Machine",
    }
}

/// Single-quote a value for PowerShell (`'` doubles inside a single-quoted
/// string) — a path is data, never script.
fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn run_powershell(script: &str) -> io::Result<()> {
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(io::Error::other(format!(
        "powershell failed ({}): {}",
        out.status,
        err.trim()
    )))
}

/// The OS home dir (NOT `MODELSTAT_HOME`) — where the startup files live.
fn os_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A missing startup file reads as empty — that is the create case, not an error.
fn read_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIN: &str = "/home/dev/.modelstat/bin";

    #[test]
    fn shell_comes_from_the_shell_var_basename() {
        assert_eq!(Shell::from_shell_var(Some("/bin/zsh")), Shell::Zsh);
        assert_eq!(Shell::from_shell_var(Some("/bin/bash")), Shell::Bash);
        assert_eq!(
            Shell::from_shell_var(Some("/usr/local/bin/fish")),
            Shell::Fish
        );
        assert_eq!(Shell::from_shell_var(Some("/bin/dash")), Shell::Posix);
        assert_eq!(Shell::from_shell_var(None), Shell::Posix);
    }

    #[test]
    fn bash_targets_bash_profile_on_macos_and_bashrc_on_linux() {
        let home = Path::new("/home/dev");
        assert!(rc_path(Shell::Bash, Os::Macos, home).ends_with(".bash_profile"));
        assert!(rc_path(Shell::Bash, Os::Linux, home).ends_with(".bashrc"));
        assert!(rc_path(Shell::Zsh, Os::Macos, home).ends_with(".zshrc"));
        assert!(rc_path(Shell::Posix, Os::Linux, home).ends_with(".profile"));
        assert!(rc_path(Shell::Fish, Os::Linux, home).ends_with("modelstat.fish"));
    }

    #[test]
    fn env_snippet_prepends_and_is_idempotent_when_sourced_twice() {
        let body = env_file_body(Path::new(BIN));
        assert!(body.contains(MARKER));
        // PREPEND — a stale `modelstat` elsewhere on PATH must not win.
        assert!(body.contains("PATH=\"/home/dev/.modelstat/bin:${PATH}\""));
        // …guarded, so sourcing it from two startup files can't grow PATH.
        assert!(body.contains("*\":/home/dev/.modelstat/bin:\"*) ;;"));
    }

    #[test]
    fn fish_snippet_prepends_and_is_guarded() {
        let body = fish_body(Path::new(BIN));
        assert!(body.contains("if not contains -- \"/home/dev/.modelstat/bin\" $PATH"));
        assert!(body.contains("set -gx PATH \"/home/dev/.modelstat/bin\" $PATH"));
    }

    #[test]
    fn apply_block_appends_once_then_reports_nothing_to_do() {
        let block = rc_block(Path::new("/home/dev/.modelstat/env"));
        let first = apply_block("export EDITOR=vim\n", &block).expect("first write");
        assert!(first.starts_with("export EDITOR=vim\n"));
        assert!(first.ends_with(&block));
        // Second run: already exactly right.
        assert_eq!(apply_block(&first, &block), None);
    }

    #[test]
    fn apply_block_repoints_a_block_from_an_older_install() {
        let old = rc_block(Path::new("/old/home/.modelstat/env"));
        let new = rc_block(Path::new("/home/dev/.modelstat/env"));
        let existing = format!("export EDITOR=vim\n\n{old}");
        let updated = apply_block(&existing, &new).expect("re-point");
        assert!(updated.contains("/home/dev/.modelstat/env"));
        assert!(!updated.contains("/old/home/.modelstat/env"), "{updated}");
        assert_eq!(updated.matches(MARKER).count(), 1, "{updated}");
    }

    #[test]
    fn apply_block_writes_into_an_empty_file_without_a_leading_blank_line() {
        let block = rc_block(Path::new("/home/dev/.modelstat/env"));
        assert_eq!(apply_block("", &block), Some(block.clone()));
    }

    #[test]
    fn strip_block_removes_only_our_two_lines() {
        let block = rc_block(Path::new("/home/dev/.modelstat/env"));
        let contents = format!("export EDITOR=vim\n\n{block}alias ll='ls -l'\n");
        let stripped = strip_block(&contents).expect("marker present");
        assert_eq!(stripped, "export EDITOR=vim\n\nalias ll='ls -l'\n");
        // Nothing of ours left → nothing more to strip.
        assert_eq!(strip_block(&stripped), None);
    }

    #[test]
    fn strip_block_leaves_a_users_own_source_lines_alone() {
        let contents = ". \"/home/dev/.other/env\"\nexport EDITOR=vim\n";
        assert_eq!(strip_block(contents), None);
    }

    #[test]
    fn ps_quote_escapes_single_quotes() {
        assert_eq!(ps_quote(r"C:\Users\dev"), r"'C:\Users\dev'");
        assert_eq!(ps_quote("it''s"), "'it''''s'");
    }

    /// Serialize the env-mutating tests — `cargo test` runs a crate's tests on
    /// threads in one process, so flipping `HOME`/`SHELL` must not interleave.
    /// (Same reason as `modelstat_ingest::test_env_lock`, which is crate-private.)
    #[cfg(unix)]
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The whole point, on a real filesystem: install → the startup file gains
    /// our block; install again → nothing changes; uninstall → the file is
    /// byte-for-byte what it was before we ever touched it.
    #[cfg(unix)]
    #[test]
    fn wiring_then_unwiring_leaves_the_startup_file_untouched() {
        let _g = env_lock();
        let root = std::env::temp_dir().join(format!("ms-path-env-{}", std::process::id()));
        let (user, msh) = (root.join("user"), root.join("home"));
        let bin = msh.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        let rc = user.join(".zshrc");
        let original = "export EDITOR=vim\n";
        std::fs::write(&rc, original).unwrap();

        let (old_home, old_shell) = (std::env::var_os("HOME"), std::env::var_os("SHELL"));
        std::env::set_var("HOME", &user);
        std::env::set_var("SHELL", "/bin/zsh");
        std::env::set_var("MODELSTAT_HOME", &msh);

        let first = ensure_on_path(&bin, Scope::User).unwrap();
        assert_eq!(first.files.len(), 2, "env snippet + startup file");
        assert!(std::fs::read_to_string(&rc).unwrap().contains(MARKER));
        assert!(msh.join("env").exists());

        // Idempotent: a re-install rewrites nothing.
        assert!(ensure_on_path(&bin, Scope::User).unwrap().files.is_empty());

        let removed = remove_from_path(&bin, Scope::User).unwrap();
        assert_eq!(removed.len(), 2, "env snippet + startup file: {removed:?}");
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), original);
        assert!(!msh.join("env").exists());

        std::env::remove_var("MODELSTAT_HOME");
        match old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match old_shell {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
