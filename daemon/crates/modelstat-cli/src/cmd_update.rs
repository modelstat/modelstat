//! `upgrade` + `autoupdate` (feature §5, §13). The MECHANISM is npm-free now
//! (binary self-replace, [`modelstat_update`]); the `autoupdate` command + its
//! `auto-update.json` state stay byte-parity with the TS daemon. `upgrade` always
//! exits 0 (TS parity — a failed upgrade is printed, not signalled via exit code).

use std::process::ExitCode;

use modelstat_update::{
    auto_update_enabled, auto_update_pinned_by_env, set_stored_auto_update, stored_auto_update,
    upgrade_now, UpgradeOutcome,
};

/// `upgrade` — manual self-update now (ignores the auto-update setting).
pub async fn cmd_upgrade() -> ExitCode {
    println!("upgrading modelstat to the latest published release…");
    match upgrade_now().await {
        UpgradeOutcome::Completed(note) => println!("  ✓ {note}"),
        UpgradeOutcome::Failed(note) => {
            println!("  {note}");
            println!("  or reinstall manually: curl -fsSL https://install.modelstat.ai | sh");
        }
        other => {
            if let Some(n) = other.note() {
                println!("  {n}");
            }
        }
    }
    ExitCode::SUCCESS
}

/// `autoupdate [on|off|toggle|status]` — show or change the stored preference.
pub fn cmd_autoupdate(args: &[String]) -> ExitCode {
    let sub = args.first().map(|s| s.to_lowercase()).unwrap_or_default();
    match sub.as_str() {
        "on" | "enable" => {
            let _ = set_stored_auto_update(true);
        }
        "off" | "disable" => {
            let _ = set_stored_auto_update(false);
        }
        "toggle" => {
            let _ = set_stored_auto_update(!stored_auto_update());
        }
        "" | "status" => {}
        _ => {
            println!("usage: modelstat autoupdate [on|off|toggle|status]");
            return ExitCode::SUCCESS;
        }
    }
    println!("auto-update: {}", if auto_update_enabled() { "on" } else { "off" });
    if auto_update_pinned_by_env() {
        println!("  (pinned by MODELSTAT_AUTO_UPDATE env — the stored toggle is ignored)");
    }
    ExitCode::SUCCESS
}
