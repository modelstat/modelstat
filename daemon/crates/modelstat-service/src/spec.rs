//! Pure service-file generation (§16) — the launchd plist, systemd unit, and
//! Windows Scheduled-Task XML for the daemon + summarizer-engine services, at
//! user or system scope. Split from the subprocess side effects (lib.rs) so the
//! byte-exact launch contract is unit-testable on any platform.
//!
//! §16 CHANGES from the TS `service.ts`: `Program = <binary> <subcommand>` — NO
//! node, NO `NODE_OPTIONS` heap knob (Rust has no V8 ceiling). Windows Scheduled
//! Task + the system scope + the summarizer engine unit are all NEW here.

use std::path::{Path, PathBuf};

/// launchd label for the collector daemon agent (`~/Library/LaunchAgents/…plist`).
pub const DAEMON_LABEL: &str = "ai.modelstat.daemon";
/// launchd label for the summarizer engine agent.
pub const SUMMARIZER_LABEL: &str = "ai.modelstat.summarizer";
/// launchd label for the macOS menu-bar tray (§15) — separate agent so the
/// pipeline survives a tray quit.
pub const TRAY_LABEL: &str = "ai.modelstat.tray";
/// The system-scope home (§16): a dedicated dir, not `~/.modelstat`.
pub const SYSTEM_HOME: &str = "/var/lib/modelstat";

/// Which unit to manage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    /// The collector daemon (`<binary> start`).
    Daemon,
    /// The summarizer engine (`<binary> serve`) — installed only in local mode /
    /// on summarizer boxes.
    Summarizer,
}

/// Install scope (§16 table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Per-user agent (launchd gui/<uid>, systemd --user, Windows Scheduled Task).
    User,
    /// System-wide (LaunchDaemon, systemd system, Windows Service) —
    /// `MODELSTAT_HOME=/var/lib/modelstat`, runs as a dedicated user/root.
    System,
}

/// A fully-resolved service unit: everything the three generators need. Built by
/// [`ServiceDef::resolve`] from a component + scope + the binaries dir + home.
#[derive(Debug, Clone)]
pub struct ServiceDef {
    pub component: Component,
    pub scope: Scope,
    /// launchd label (`ai.modelstat.daemon` / `ai.modelstat.summarizer`).
    pub label: &'static str,
    /// systemd unit base + Windows task name (`modelstat` / `modelstat-summarizer`).
    pub unit_name: &'static str,
    /// Human description for the unit/task.
    pub description: &'static str,
    /// The absolute binary path.
    pub binary: PathBuf,
    /// The subcommand the supervisor runs (`start` / `serve`).
    pub subcommand: &'static str,
    /// `MODELSTAT_HOME` for this scope (drives the log dir).
    pub home: PathBuf,
    pub out_log: PathBuf,
    pub err_log: PathBuf,
    /// launchd `ThrottleInterval` (daemon 30s; §16).
    pub throttle: u32,
}

impl ServiceDef {
    /// Resolve a unit. `bin_dir` holds `modelstat`(`.exe`) + `modelstat-summarizer`;
    /// `home` is the scope's `MODELSTAT_HOME` (`~/.modelstat` for user;
    /// `/var/lib/modelstat` for system).
    pub fn resolve(component: Component, scope: Scope, bin_dir: &Path, home: &Path) -> Self {
        let (label, unit_name, description, subcommand, bin_base, log_prefix, throttle) =
            match component {
                Component::Daemon => (
                    DAEMON_LABEL,
                    "modelstat",
                    "modelstat daemon",
                    "start",
                    "modelstat",
                    "",
                    30,
                ),
                Component::Summarizer => (
                    SUMMARIZER_LABEL,
                    "modelstat-summarizer",
                    "modelstat summarizer engine",
                    "serve",
                    "modelstat-summarizer",
                    "summarizer-",
                    30,
                ),
            };
        let logs = home.join("logs");
        ServiceDef {
            component,
            scope,
            label,
            unit_name,
            description,
            binary: bin_dir.join(exe_name(bin_base)),
            subcommand,
            home: home.to_path_buf(),
            out_log: logs.join(format!("{log_prefix}out.log")),
            err_log: logs.join(format!("{log_prefix}err.log")),
            throttle,
        }
    }

    /// The launchd plist body (§16 — no node, no `NODE_OPTIONS`).
    pub fn launchd_plist(&self) -> String {
        let mut env = String::from(
            "    <key>PATH</key><string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>\n",
        );
        if self.scope == Scope::System {
            env.push_str(&format!(
                "    <key>MODELSTAT_HOME</key><string>{}</string>\n",
                SYSTEM_HOME
            ));
        }
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin}</string>
    <string>{sub}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>{throttle}</integer>
  <key>StandardOutPath</key><string>{out}</string>
  <key>StandardErrorPath</key><string>{err}</string>
  <key>EnvironmentVariables</key>
  <dict>
{env}  </dict>
  <key>WorkingDirectory</key><string>{home}</string>
</dict>
</plist>
"#,
            label = self.label,
            bin = self.binary.display(),
            sub = self.subcommand,
            throttle = self.throttle,
            out = self.out_log.display(),
            err = self.err_log.display(),
            env = env,
            home = self.home.display(),
        )
    }

    /// The systemd unit body (§16 — no node, no `NODE_OPTIONS`; system scope
    /// carries `MODELSTAT_HOME` + targets multi-user).
    pub fn systemd_unit(&self) -> String {
        let env_line = if self.scope == Scope::System {
            format!("Environment=MODELSTAT_HOME={SYSTEM_HOME}\n")
        } else {
            String::new()
        };
        let wanted_by = match self.scope {
            Scope::User => "default.target",
            Scope::System => "multi-user.target",
        };
        format!(
            "[Unit]\n\
             Description={desc}\n\
             Documentation=https://modelstat.ai\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             {env_line}\
             ExecStart={bin} {sub}\n\
             Restart=always\n\
             RestartSec=10\n\
             StartLimitIntervalSec=300\n\
             StartLimitBurst=10\n\
             StandardOutput=append:{out}\n\
             StandardError=append:{err}\n\
             \n\
             [Install]\n\
             WantedBy={wanted_by}\n",
            desc = self.description,
            env_line = env_line,
            bin = self.binary.display(),
            sub = self.subcommand,
            out = self.out_log.display(),
            err = self.err_log.display(),
            wanted_by = wanted_by,
        )
    }

    /// The Windows Task Scheduler XML (§16, NEW): run at log-on (user scope) or
    /// startup, hidden window, restart on failure ×3 / 1 min, no time limit.
    pub fn schtasks_xml(&self) -> String {
        // System scope runs at boot as a service-like task; user scope at logon.
        let (trigger, logon_type, run_level) = match self.scope {
            Scope::User => (
                "    <LogonTrigger><Enabled>true</Enabled></LogonTrigger>",
                "InteractiveToken",
                "LeastPrivilege",
            ),
            Scope::System => (
                "    <BootTrigger><Enabled>true</Enabled></BootTrigger>",
                "ServiceAccount",
                "HighestAvailable",
            ),
        };
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>{desc}</Description>
    <URI>\{name}</URI>
  </RegistrationInfo>
  <Triggers>
{trigger}
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>{logon_type}</LogonType>
      <RunLevel>{run_level}</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{bin}</Command>
      <Arguments>{sub}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
            desc = self.description,
            name = self.unit_name,
            trigger = trigger,
            logon_type = logon_type,
            run_level = run_level,
            bin = self.binary.display(),
            sub = self.subcommand,
        )
    }
}

/// Append `.exe` on Windows targets.
fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon_user() -> ServiceDef {
        ServiceDef::resolve(
            Component::Daemon,
            Scope::User,
            Path::new("/home/dev/.modelstat/bin"),
            Path::new("/home/dev/.modelstat"),
        )
    }

    #[test]
    fn launchd_plist_runs_the_binary_directly_no_node() {
        let plist = daemon_user().launchd_plist();
        assert!(plist.contains("<key>Label</key><string>ai.modelstat.daemon</string>"));
        assert!(plist.contains("<string>/home/dev/.modelstat/bin/modelstat</string>"));
        assert!(plist.contains("<string>start</string>"));
        assert!(plist.contains("<key>ThrottleInterval</key><integer>30</integer>"));
        assert!(plist.contains("<key>SuccessfulExit</key><false/>"));
        // §16: no node, no NODE_OPTIONS.
        assert!(!plist.contains("node"), "{plist}");
        assert!(!plist.contains("NODE_OPTIONS"), "{plist}");
        // User scope carries no MODELSTAT_HOME override.
        assert!(!plist.contains("MODELSTAT_HOME"));
    }

    #[test]
    fn systemd_unit_execstarts_binary_start_no_node() {
        let unit = daemon_user().systemd_unit();
        assert!(unit.contains("ExecStart=/home/dev/.modelstat/bin/modelstat start"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("RestartSec=10"));
        assert!(unit.contains("StartLimitIntervalSec=300"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("node"), "{unit}");
        assert!(!unit.contains("NODE_OPTIONS"), "{unit}");
    }

    #[test]
    fn summarizer_unit_serves_and_logs_to_summarizer_files() {
        let def = ServiceDef::resolve(
            Component::Summarizer,
            Scope::User,
            Path::new("/b"),
            Path::new("/h"),
        );
        let unit = def.systemd_unit();
        assert!(unit.contains("ExecStart=/b/modelstat-summarizer serve"));
        assert!(unit.contains("append:/h/logs/summarizer-out.log"));
        assert!(unit.contains("append:/h/logs/summarizer-err.log"));
        assert_eq!(def.label, "ai.modelstat.summarizer");
    }

    #[test]
    fn system_scope_adds_modelstat_home_and_multi_user_target() {
        let def = ServiceDef::resolve(
            Component::Daemon,
            Scope::System,
            Path::new("/usr/local/bin"),
            Path::new("/var/lib/modelstat"),
        );
        let unit = def.systemd_unit();
        assert!(unit.contains("Environment=MODELSTAT_HOME=/var/lib/modelstat"));
        assert!(unit.contains("WantedBy=multi-user.target"));
        let plist = def.launchd_plist();
        assert!(plist.contains("<key>MODELSTAT_HOME</key><string>/var/lib/modelstat</string>"));
    }

    #[test]
    fn schtasks_xml_runs_binary_with_restart_and_hidden() {
        let xml = daemon_user().schtasks_xml();
        assert!(xml.contains("<Command>/home/dev/.modelstat/bin/modelstat</Command>"));
        assert!(xml.contains("<Arguments>start</Arguments>"));
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<Hidden>true</Hidden>"));
        assert!(xml.contains("<Count>3</Count>"));
        // System scope switches to a boot trigger.
        let sys = ServiceDef::resolve(
            Component::Daemon,
            Scope::System,
            Path::new("/b"),
            Path::new("/var/lib/modelstat"),
        )
        .schtasks_xml();
        assert!(sys.contains("<BootTrigger>"));
    }
}
