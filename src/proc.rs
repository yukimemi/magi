//! Spawning child processes without putting a window on the operator's screen.
//!
//! Every external program magi runs - the agent CLIs, `git`, `gh`, the
//! configured verification commands - is a console application. What happens
//! when one is spawned depends on whether the *parent* has a console, and
//! magi has two kinds of parent:
//!
//! - `magi run` / `magi review` in a terminal. The child inherits that
//!   console, writes nowhere visible because its pipes are redirected, and
//!   nothing appears.
//! - `magi web`, which serves the deck. Its successor is spawned
//!   `DETACHED_PROCESS` on purpose (see [`crate::web`]): it has to outlive the
//!   process that started it and must not hold a pipe a terminal is waiting
//!   on. **That process has no console at all**, so Windows allocates a brand
//!   new one for each console child - and draws it. An implement wave is
//!   three agents, so three black windows opened over whatever the operator
//!   was doing, in front of the browser they were reading the deck in.
//!
//! `CREATE_NO_WINDOW` is the answer to exactly that: the child still gets a
//! console for its standard handles, and that console is never shown. It is
//! not the same as `DETACHED_PROCESS`, which gives the child no console and
//! would make a grandchild pop a window of its own for the same reason.
//!
//! Nothing here is conditional on how magi was started. A hidden console is
//! correct in a terminal too: the pipes are redirected either way, so there
//! was never anything to look at.

/// `CREATE_NO_WINDOW` - run the child's console, but never draw it.
///
/// From `processthreadsapi.h`. Spelled out rather than pulled in from a
/// bindings crate: it is one number that has been stable since Windows 2000,
/// and the alternative is a dependency for it.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Spawn without a visible console window.
///
/// Implemented for both `Command` types magi uses - `std` for the few
/// synchronous calls, `tokio` for everything else - so a call site does not
/// have to know which one it is holding, and so no call site has to repeat a
/// `#[cfg(windows)]` block to get it.
///
/// A no-op off Windows, where a spawned process has no window to begin with.
pub trait Quiet {
    /// Apply it, and hand the command back for further building.
    fn quiet(&mut self) -> &mut Self;
}

impl Quiet for std::process::Command {
    fn quiet(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

impl Quiet for tokio::process::Command {
    fn quiet(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

/// Best-effort liveness check for a process id, with no dependency beyond
/// what the platform ships.
///
/// There is no portable way in the standard library to ask "is this pid
/// alive" - no `libc`, no `sysinfo`, nothing magi already depends on binds
/// the signals API - so this shells out to whatever each platform already
/// provides: `kill -0` on Unix, `tasklist` on Windows. Both are read-only:
/// `kill -0` sends no signal, it only checks whether one *could* be sent.
///
/// Every uncertain outcome reads as alive, on purpose. This exists so
/// [`crate::daemon::sweep_stale_claims`] can reclaim a lock faster than its
/// age-based fallback when the owning process is verifiably gone; the risk
/// on the other side - reclaiming a lock a live process still holds - lets a
/// second daemon start a second run on the same task, which costs far more
/// than leaving one lock alone a little longer. So a helper program that is
/// missing, output that cannot be parsed, or a permission error that merely
/// proves the pid exists under another account, all count as "alive" rather
/// than as license to reclaim.
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        match std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
        {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                // "No such process" is the one answer that actually means the
                // pid is gone. Anything else - most commonly "Operation not
                // permitted" for a pid that exists under another account - is
                // not evidence of that.
                let stderr = String::from_utf8_lossy(&o.stderr).to_lowercase();
                !stderr.contains("no such process")
            }
            Err(_) => true,
        }
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .quiet()
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\""))
            }
            _ => true,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag is the one Windows documents, and not one of the two it is
    /// easily confused with.
    ///
    /// `DETACHED_PROCESS` (0x8) is what leaves a process without a console -
    /// which is what caused the windows this module exists to stop, because a
    /// child of such a process gets a fresh console *with* a window.
    /// `CREATE_NEW_CONSOLE` (0x10) asks for the window outright.
    #[cfg(windows)]
    #[test]
    fn the_flag_hides_a_console_rather_than_removing_or_creating_one() {
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
        assert_ne!(CREATE_NO_WINDOW, 0x0000_0008, "DETACHED_PROCESS");
        assert_ne!(CREATE_NO_WINDOW, 0x0000_0010, "CREATE_NEW_CONSOLE");
    }

    /// Applying it does not disturb the command being built.
    ///
    /// The trait returns `&mut Self` so it can sit in the middle of a builder
    /// chain, and a call site that put it there must not lose its program or
    /// arguments to it.
    #[test]
    fn quiet_leaves_the_command_it_was_handed_intact() {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(["status", "--short"]).quiet();
        let built = cmd.as_std();
        assert_eq!(built.get_program(), "git");
        let args: Vec<_> = built.get_args().collect();
        assert_eq!(args, ["status", "--short"]);
    }

    #[test]
    fn this_process_is_alive_and_a_pid_nothing_ever_reuses_is_not() {
        assert!(pid_alive(std::process::id()), "this test is running");
        // Not `u32::MAX`: Windows' `tasklist` answers a pid that large with
        // "invalid query" rather than "no such process", which this helper
        // - correctly - cannot tell apart from a check it simply could not
        // run, so it reads as alive. A pid past any real process table but
        // still a value `tasklist` accepts as a query is the one this test
        // can assert on without racing whatever else is running on the
        // machine.
        assert!(!pid_alive(999_999_999));
    }
}
