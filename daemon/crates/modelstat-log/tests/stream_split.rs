//! The one thing a unit test cannot check: that `emit` writes to the file
//! descriptor `goes_to_stdout` names.
//!
//! `goes_to_stdout` is a pure table and is asserted directly in the crate's own
//! tests. But the whole point of that table is what launchd and systemd do with
//! the two streams afterwards — INFO into `out.log`, WARN/ERROR into `err.log` —
//! and a crossed pair of `write_all` calls would satisfy every unit test while
//! putting every warning in the wrong file. So this re-executes the test binary
//! as a child with its two streams held apart, and reads back which one each
//! line actually came out of.

use std::process::Command;

/// Set on the child so it takes the probe branch instead of recursing.
const PROBE: &str = "MODELSTAT_LOG_STREAM_PROBE";

/// Emit one line per level in each supervised mode, then exit. Runs only in the
/// child process. The markers are deliberately unlike anything the test harness
/// prints, so matching them can't collide with `test … ok` progress output.
fn probe() {
    modelstat_log::init_service();
    modelstat_log::log_info!("probe-service-info");
    modelstat_log::log_warn!("probe-service-warn");
    modelstat_log::log_error!("probe-service-error");

    modelstat_log::init_service_stdout_reserved();
    modelstat_log::log_info!("probe-reserved-info");
}

#[test]
fn info_goes_out_the_stdout_fd_and_warnings_go_out_the_stderr_fd() {
    if std::env::var_os(PROBE).is_some() {
        probe();
        return;
    }

    let exe = std::env::current_exe().expect("test binary path");
    let run = Command::new(exe)
        .env(PROBE, "1")
        // `--nocapture` matters: without it the harness swallows the child's
        // streams and both sides come back empty, which would pass every
        // negative assertion below for the wrong reason.
        .args([
            "--exact",
            "info_goes_out_the_stdout_fd_and_warnings_go_out_the_stderr_fd",
            "--nocapture",
        ])
        .output()
        .expect("re-exec the test binary");

    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();

    // The child must actually have run — otherwise every `!contains` below is
    // vacuously true and this test guards nothing.
    assert!(
        stdout.contains("probe-service-info"),
        "child produced no INFO on stdout.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // A service's routine narration is the only thing on stdout…
    assert!(
        !stdout.contains("probe-service-warn") && !stdout.contains("probe-service-error"),
        "a warning or error leaked into out.log's stream:\n{stdout}"
    );
    // …and every warning and error is on stderr.
    assert!(
        stderr.contains("probe-service-warn"),
        "WARN missing from stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("probe-service-error"),
        "ERROR missing from stderr:\n{stderr}"
    );
    // The daemon's INFO must NOT be duplicated onto stderr — err.log staying
    // free of routine noise is the entire reason this split was asked for.
    assert!(
        !stderr.contains("probe-service-info"),
        "INFO reached err.log's stream too:\n{stderr}"
    );

    // With stdout reserved (MCP JSON-RPC frames, `_daemon-health`'s document),
    // even INFO stays on stderr. One log line on those streams is a parse error.
    assert!(
        !stdout.contains("probe-reserved-info"),
        "an INFO line landed on a stdout that is carrying data:\n{stdout}"
    );
    assert!(
        stderr.contains("probe-reserved-info"),
        "reserved-mode INFO went nowhere:\n{stderr}"
    );
}
