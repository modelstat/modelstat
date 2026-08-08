//! Run the collector at background priority.
//!
//! modelstat is a bystander on someone else's machine. It redacts transcripts
//! with an on-device model, and after a version bump it re-reads every session
//! ever recorded — hours of genuine CPU work that the user did not ask for and
//! is not waiting on. At default priority that work competes on equal terms with
//! the editor they are typing into, which is how a background collector ends up
//! at the top of Activity Monitor and gets reported as a bug.
//!
//! Lowering priority does not make the daemon slower in any way a user notices:
//!
//!   · on an idle machine it changes nothing at all — a lower-priority process
//!     still gets every free cycle;
//!   · under contention it yields to the foreground app, which is exactly the
//!     behaviour we want and the opposite of what we shipped;
//!   · it does not touch I/O or the network, so upload latency is untouched.
//!     The requirement that data ships promptly is unaffected — this is a CPU
//!     scheduling hint and nothing else.
//!
//! Applied to the process rather than configured in a launchd plist / systemd
//! unit so it holds however the daemon was started: supervised, `brew services`,
//! or a person running `modelstat start` in a terminal.

/// The nice value the collector runs at (Unix). 10 is the conventional "batch
/// work" level — clearly below interactive, well above the 19 that can starve a
/// process for minutes at a time on a busy machine.
#[cfg(unix)]
const BACKGROUND_NICE: i32 = 10;

/// Drop this process to background CPU priority.
///
/// Best-effort by nature: a sandbox may refuse, and on Unix a process that was
/// already niced cannot renice itself back down. A failure means the daemon runs
/// at normal priority — the behaviour it had before this existed — so it is
/// logged and carried past rather than treated as fatal. It is not a
/// silent degrade: nothing about correctness or what gets uploaded depends on it.
pub fn run_in_background() {
    #[cfg(unix)]
    {
        // `setpriority` returns 0 or -1, so -1 is unambiguously an error and needs
        // no errno dance. (That dance is `getpriority`'s problem: its return value
        // IS the nice level, and -1 is a legal one.)
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, BACKGROUND_NICE) };
        if rc == -1 {
            let err = std::io::Error::last_os_error();
            modelstat_log::log_warn!(
                "could not lower the collector's CPU priority ({err}) — it will \
                 run at normal priority and may compete with foreground apps"
            );
            return;
        }
        modelstat_log::log_info!(
            "running at background CPU priority (nice {BACKGROUND_NICE}) — \
             foreground apps get the machine first"
        );
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS,
        };
        // BELOW_NORMAL rather than IDLE: IDLE_PRIORITY_CLASS on Windows can be
        // starved indefinitely by any busy normal-priority process, which would
        // stall a backfill for as long as the machine is in use.
        let ok = unsafe { SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS) };
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            modelstat_log::log_warn!(
                "could not lower the collector's CPU priority ({err}) — it will run \
                 at normal priority and may compete with foreground apps"
            );
            return;
        }
        modelstat_log::log_info!(
            "running at below-normal CPU priority — foreground apps get the machine first"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The observable effect, checked against the OS rather than against our own
    /// return value: after the call, this process really is niced.
    ///
    /// Runs in-process and is therefore one-way — nothing later in the test binary
    /// can undo it. That is harmless (tests do not race the scheduler) and it is
    /// the only honest way to test a process-global setting.
    #[test]
    fn the_process_really_is_deprioritised_afterwards() {
        let before = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        run_in_background();
        let after = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        assert!(
            after >= before,
            "priority must never go UP: {before} → {after}"
        );
        // On any normal CI/dev box this starts at 0 and lands on the constant. A
        // sandbox that refuses the call leaves it where it was, which the
        // monotonic assertion above already covers.
        if before <= BACKGROUND_NICE {
            assert_eq!(after, BACKGROUND_NICE, "expected nice {BACKGROUND_NICE}");
        }
    }
}
