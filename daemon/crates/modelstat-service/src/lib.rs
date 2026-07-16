//! Service management (§16) + macOS tray (§15): install / uninstall / status for
//! the collector daemon + summarizer engine, across launchd (macOS), systemd
//! (Linux), and Windows Scheduled Task / Service — at user or system scope. Plus
//! the legacy-launcher cleanup and the tray agent.
//!
//! The byte-exact file bodies live in [`spec`] (pure, fully tested); this module
//! is the subprocess side (launchctl / systemctl / schtasks / sc) + the paths,
//! which only exercise on the real OS. `install_service` returns `{path, logs}`;
//! `service_status` returns `{running, hint}` with the §16 per-platform hints.

pub mod legacy;
pub mod spec;
pub mod tray;

use std::path::PathBuf;
use std::process::Command;

use modelstat_ingest::{home_path, modelstat_home};

pub use spec::{
    Component, Scope, ServiceDef, DAEMON_LABEL, SUMMARIZER_LABEL, SYSTEM_HOME, TRAY_LABEL,
};

/// Where `install_service` wrote the unit + where its logs land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInstall {
    pub path: PathBuf,
    pub logs: PathBuf,
}

/// Whether a managed service is running + a human hint (§16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub running: bool,
    pub hint: String,
}

/// The installed-binaries dir (`~/.modelstat/bin`) — where `_setup-runtime`
/// stages `modelstat`(+`.exe`) and `modelstat-summarizer`.
pub fn bin_dir() -> PathBuf {
    home_path("bin")
}

/// `MODELSTAT_HOME` for a scope: `~/.modelstat` (user) or `/var/lib/modelstat`
/// (system, §16).
fn scope_home(scope: Scope) -> PathBuf {
    match scope {
        Scope::User => modelstat_home(),
        Scope::System => PathBuf::from(SYSTEM_HOME),
    }
}

/// Resolve the unit for a component + scope against the real install paths.
pub fn service_def(component: Component, scope: Scope) -> ServiceDef {
    ServiceDef::resolve(component, scope, &bin_dir(), &scope_home(scope))
}

/// The OS home (NOT `MODELSTAT_HOME`) — where launchd LaunchAgents live.
fn os_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The on-disk service-file path for a unit (where the plist / unit / task-xml is
/// written). Pure — no side effects — so it's returned by `install_service` and
/// used by `uninstall_service`.
pub fn service_file_path(def: &ServiceDef) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        match def.scope {
            Scope::User => os_home()
                .join("Library/LaunchAgents")
                .join(format!("{}.plist", def.label)),
            Scope::System => {
                PathBuf::from("/Library/LaunchDaemons").join(format!("{}.plist", def.label))
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        match def.scope {
            Scope::User => {
                let xdg = std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| os_home().join(".config"));
                xdg.join("systemd/user")
                    .join(format!("{}.service", def.unit_name))
            }
            Scope::System => {
                PathBuf::from("/etc/systemd/system").join(format!("{}.service", def.unit_name))
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        // The task XML we stage before registering it (schtasks reads it by path).
        home_path("service").join(format!("{}.xml", def.unit_name))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = def;
        PathBuf::from("/dev/null")
    }
}

/// The unit's log directory (`<scope home>/logs`).
pub fn logs_dir(def: &ServiceDef) -> PathBuf {
    def.home.join("logs")
}

fn run(program: &str, args: &[&str]) -> (bool, String, String) {
    match Command::new(program).args(args).output() {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(e) => (false, String::new(), e.to_string()),
    }
}

/// Install (and start) the service for a component + scope. Idempotent: an
/// existing instance is unloaded + reloaded so it picks up a fresh binary. Port
/// of `installService` (macInstall / linuxInstall) + the §16 Windows path.
pub fn install_service(component: Component, scope: Scope) -> std::io::Result<ServiceInstall> {
    let def = service_def(component, scope);
    let path = service_file_path(&def);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::create_dir_all(logs_dir(&def))?;

    #[cfg(target_os = "macos")]
    {
        std::fs::write(&path, def.launchd_plist())?;
        mac_load(&def, &path)?;
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::write(&path, def.systemd_unit())?;
        linux_load(&def)?;
    }
    #[cfg(target_os = "windows")]
    {
        std::fs::write(&path, def.schtasks_xml())?;
        windows_load(&def, &path)?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "Service installation isn't supported on {}. Run 'modelstat start' manually.",
                std::env::consts::OS
            ),
        ));
    }
    Ok(ServiceInstall {
        path,
        logs: logs_dir(&def),
    })
}

/// Uninstall (stop + remove) the service for a component + scope. Best-effort:
/// the unit file is removed even if the stop command reports an error.
pub fn uninstall_service(component: Component, scope: Scope) -> std::io::Result<()> {
    let def = service_def(component, scope);
    let path = service_file_path(&def);
    #[cfg(target_os = "macos")]
    mac_unload(&def);
    #[cfg(target_os = "linux")]
    linux_unload(&def);
    #[cfg(target_os = "windows")]
    windows_unload(&def);
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

/// Whether the service is running + a per-platform hint (§16).
pub fn service_status(component: Component, scope: Scope) -> ServiceStatus {
    let def = service_def(component, scope);
    #[cfg(target_os = "macos")]
    {
        return mac_status(&def);
    }
    #[cfg(target_os = "linux")]
    {
        return linux_status(&def);
    }
    #[cfg(target_os = "windows")]
    {
        return windows_status(&def);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = def;
        ServiceStatus {
            running: false,
            hint: format!("unsupported platform ({})", std::env::consts::OS),
        }
    }
}

// ── macOS (launchd) ──────────────────────────────────────────────────────
#[cfg(target_os = "macos")]
fn launchd_domain(scope: Scope) -> String {
    match scope {
        Scope::User => format!("gui/{}", unsafe { libc::getuid() }),
        Scope::System => "system".to_string(),
    }
}

#[cfg(target_os = "macos")]
fn mac_load(def: &ServiceDef, path: &std::path::Path) -> std::io::Result<()> {
    let domain = launchd_domain(def.scope);
    let target = format!("{domain}/{}", def.label);
    // Idempotent: unload the previous instance, then bootstrap + kickstart.
    run("launchctl", &["bootout", &target]);
    let (ok, _out, err) = run("launchctl", &["bootstrap", &domain, &path.to_string_lossy()]);
    if !ok {
        return Err(std::io::Error::other(format!(
            "launchctl bootstrap failed: {}",
            err.trim()
        )));
    }
    run("launchctl", &["kickstart", "-k", &target]);
    Ok(())
}

#[cfg(target_os = "macos")]
fn mac_unload(def: &ServiceDef) {
    let target = format!("{}/{}", launchd_domain(def.scope), def.label);
    run("launchctl", &["bootout", &target]);
}

#[cfg(target_os = "macos")]
fn mac_status(def: &ServiceDef) -> ServiceStatus {
    let target = format!("{}/{}", launchd_domain(def.scope), def.label);
    let (ok, _o, _e) = run("launchctl", &["print", &target]);
    ServiceStatus {
        running: ok,
        hint: if ok { "launchd managed" } else { "not installed" }.to_string(),
    }
}

// ── Linux (systemd) ──────────────────────────────────────────────────────
#[cfg(target_os = "linux")]
fn systemctl(scope: Scope, rest: &[&str]) -> (bool, String, String) {
    let mut args: Vec<&str> = Vec::new();
    if scope == Scope::User {
        args.push("--user");
    }
    args.extend_from_slice(rest);
    run("systemctl", &args)
}

#[cfg(target_os = "linux")]
fn linux_load(def: &ServiceDef) -> std::io::Result<()> {
    let unit = format!("{}.service", def.unit_name);
    systemctl(def.scope, &["daemon-reload"]);
    let (ok, _o, err) = systemctl(def.scope, &["enable", "--now", &unit]);
    if !ok {
        return Err(std::io::Error::other(format!(
            "systemctl enable failed: {}",
            err.trim()
        )));
    }
    systemctl(def.scope, &["restart", &unit]);
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_unload(def: &ServiceDef) {
    let unit = format!("{}.service", def.unit_name);
    systemctl(def.scope, &["disable", "--now", &unit]);
    systemctl(def.scope, &["daemon-reload"]);
}

#[cfg(target_os = "linux")]
fn linux_status(def: &ServiceDef) -> ServiceStatus {
    let unit = format!("{}.service", def.unit_name);
    let (_ok, out, _e) = systemctl(def.scope, &["is-active", &unit]);
    let active = out.trim() == "active";
    ServiceStatus {
        running: active,
        hint: if active { "systemd managed" } else { "not running" }.to_string(),
    }
}

// ── Windows (Scheduled Task / Service) ───────────────────────────────────
#[cfg(target_os = "windows")]
fn windows_load(def: &ServiceDef, xml_path: &std::path::Path) -> std::io::Result<()> {
    match def.scope {
        Scope::User => {
            run("schtasks", &["/End", "/TN", def.unit_name]);
            let (ok, _o, err) = run(
                "schtasks",
                &[
                    "/Create",
                    "/TN",
                    def.unit_name,
                    "/XML",
                    &xml_path.to_string_lossy(),
                    "/F",
                ],
            );
            if !ok {
                return Err(std::io::Error::other(format!(
                    "schtasks /Create failed: {}",
                    err.trim()
                )));
            }
            run("schtasks", &["/Run", "/TN", def.unit_name]);
            Ok(())
        }
        Scope::System => {
            let bin_path = format!("\"{}\" {}", def.binary.display(), def.subcommand);
            run("sc", &["stop", def.unit_name]);
            run("sc", &["delete", def.unit_name]);
            let (ok, _o, err) = run(
                "sc",
                &[
                    "create",
                    def.unit_name,
                    "binPath=",
                    &bin_path,
                    "start=",
                    "auto",
                    "DisplayName=",
                    def.description,
                ],
            );
            if !ok {
                return Err(std::io::Error::other(format!(
                    "sc create failed: {}",
                    err.trim()
                )));
            }
            run(
                "sc",
                &["failure", def.unit_name, "reset=", "0", "actions=", "restart/10000"],
            );
            run("sc", &["start", def.unit_name]);
            Ok(())
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_unload(def: &ServiceDef) {
    match def.scope {
        Scope::User => {
            run("schtasks", &["/End", "/TN", def.unit_name]);
            run("schtasks", &["/Delete", "/TN", def.unit_name, "/F"]);
        }
        Scope::System => {
            run("sc", &["stop", def.unit_name]);
            run("sc", &["delete", def.unit_name]);
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_status(def: &ServiceDef) -> ServiceStatus {
    match def.scope {
        Scope::User => {
            let (ok, out, _e) = run("schtasks", &["/Query", "/TN", def.unit_name]);
            let running = ok && out.contains("Running");
            ServiceStatus {
                running,
                hint: if ok { "scheduled task" } else { "not installed" }.to_string(),
            }
        }
        Scope::System => {
            let (ok, out, _e) = run("sc", &["query", def.unit_name]);
            let running = ok && out.contains("RUNNING");
            ServiceStatus {
                running,
                hint: if ok { "windows service" } else { "not installed" }.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_and_system_resolve_to_distinct_files() {
        let user = service_file_path(&service_def(Component::Daemon, Scope::User));
        let system = service_file_path(&service_def(Component::Daemon, Scope::System));
        assert_ne!(user, system);
        #[cfg(target_os = "macos")]
        {
            assert!(user.to_string_lossy().contains("LaunchAgents"));
            assert!(system.to_string_lossy().contains("LaunchDaemons"));
            assert!(user.to_string_lossy().ends_with("ai.modelstat.daemon.plist"));
        }
        #[cfg(target_os = "linux")]
        {
            assert!(user.to_string_lossy().contains("systemd/user"));
            assert!(system.to_string_lossy().starts_with("/etc/systemd/system"));
            assert!(user.to_string_lossy().ends_with("modelstat.service"));
        }
    }

    #[test]
    fn summarizer_and_daemon_resolve_to_distinct_units() {
        let d = service_file_path(&service_def(Component::Daemon, Scope::User));
        let s = service_file_path(&service_def(Component::Summarizer, Scope::User));
        assert_ne!(d, s);
    }
}
