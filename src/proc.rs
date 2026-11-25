//! Spawning child processes without putting a window on the operator's screen
//! or creating a fresh Windows console host (`conhost.exe`) for every child.
//!
//! Every external program magi runs - the agent CLIs, `git`, `gh`, the
//! configured verification commands - is a console application. On Windows,
//! `CreateProcessW` handles console attachment in two distinct ways:
//!
//! - `CREATE_NO_WINDOW` (`0x0800_0000`) always allocates a brand-new hidden
//!   console (spawning a new `conhost.exe`) for the child, **even when the
//!   parent already has a console**. That prevents visible windows, but in a
//!   long-lived process running hundreds of `git` calls it doubles the number
//!   of process creations (one `conhost.exe` per child).
//! - Omitting `CREATE_NO_WINDOW` (passing `CREATE_NEW_PROCESS_GROUP` instead)
//!   lets the child inherit the parent's existing console and starts **zero**
//!   extra `conhost.exe` processes. If the parent's own console is hidden,
//!   both children and grandchildren (`git-remote-https`, credential helpers,
//!   `sh` for hooks, agent CLIs) inherit that hidden console and never show a
//!   window.
//!
//! magi therefore splits Windows execution into two modes:
//!
//! - **Detached / console-less processes** (`web`, scheduler, execution
//!   workers, watch supervisors): each process holds one private hidden
//!   console for its entire lifetime. When magi spawns one of these itself
//!   ([`configure_detached_hidden_console`]), it passes
//!   `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP` (so the child gets its own
//!   private hidden console, detached from the caller's terminal) and sets an
//!   internal marker (`MAGI_HIDDEN_CONSOLE=1`). When launched externally
//!   without a console (`GetConsoleProcessList == 0`, e.g. Task Scheduler,
//!   `DETACHED_PROCESS`, or an older magi binary), [`init_hidden_console`]
//!   runs single-threaded at startup before worker threads exist and
//!   dynamically resolves `AllocConsoleWithOptions(ALLOC_CONSOLE_MODE_NO_WINDOW)`
//!   while saving and restoring standard handles. `AllocConsole` +
//!   `ShowWindow(SW_HIDE)` is never used because it can briefly flash a window;
//!   if `AllocConsoleWithOptions` is unavailable (older Windows), the process
//!   falls back cleanly to `CREATE_NO_WINDOW` per child.
//!
//!   Once shared hidden-console mode is active, the parent registers a
//!   non-inheritable `SetConsoleCtrlHandler` that swallows `CTRL_C_EVENT` and
//!   `CTRL_BREAK_EVENT` (leaving logoff/shutdown events to OS defaults), and
//!   [`Quiet`] / [`apply_isolated_child_flags`] spawn children with
//!   `CREATE_NEW_PROCESS_GROUP` instead of `CREATE_NO_WINDOW` (stripping
//!   `MAGI_HIDDEN_CONSOLE`). Each child gets its own process group (isolating
//!   targeted `CTRL_BREAK_EVENT` and suppressing broadcast `CTRL_C_EVENT` on
//!   siblings) while inheriting the single hidden console.
//!
//! - **Terminal invocations** (`magi run`, `magi review`, interactive `magi
//!   web` / `magi serve` from a shell): these keep shared hidden-console mode
//!   off and continue passing `CREATE_NO_WINDOW` to children so child
//!   processes cannot disturb the operator's terminal state or Ctrl-C
//!   handling.

/// `CREATE_NO_WINDOW` - create a new hidden console for the child.
///
/// From `processthreadsapi.h`. Spelled out rather than pulled in from a
/// bindings crate: it is one number that has been stable since Windows 2000,
/// and the alternative is a dependency for it.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `CREATE_NEW_PROCESS_GROUP` - give the child its own console process group
/// and disable default `CTRL_C_EVENT` handling on the child.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// `CREATE_SUSPENDED` - start the primary thread suspended so the child can
/// be assigned to a Windows Job Object before executing any code.
#[cfg(windows)]
const CREATE_SUSPENDED: u32 = 0x0000_0004;

/// Creation flags for a long-lived detached magi process (web successor,
/// worker, watch supervisor): one private hidden console detached from the
/// caller's terminal, in its own process group.
#[cfg(windows)]
pub(crate) const DETACHED_HIDDEN_CONSOLE_FLAGS: u32 = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;

/// Internal marker passed only by [`configure_detached_hidden_console`] so a
/// newly started magi process can verify it owns a private hidden console.
#[cfg(windows)]
pub(crate) const HIDDEN_CONSOLE_ENV: &str = "MAGI_HIDDEN_CONSOLE";

#[cfg(windows)]
mod win_console {
    use std::ffi::{OsStr, c_void};
    use std::sync::Once;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::HIDDEN_CONSOLE_ENV;

    const STD_INPUT_HANDLE: u32 = -10_i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11_i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12_i32 as u32;
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const ALLOC_CONSOLE_MODE_NO_WINDOW: u32 = 2;
    const ALLOC_CONSOLE_RESULT_NEW_CONSOLE: u32 = 1;

    #[repr(C)]
    struct AllocConsoleOptions {
        mode: u32,
        use_show_window: i32,
        show_window: u16,
    }

    type AllocConsoleWithOptionsFn =
        unsafe extern "system" fn(*const AllocConsoleOptions, *mut u32) -> i32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleProcessList(process_list: *mut u32, process_count: u32) -> u32;
        fn GetConsoleWindow() -> *mut c_void;
        fn GetModuleHandleW(module_name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, proc_name: *const u8) -> *mut c_void;
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
        fn SetStdHandle(std_handle: u32, handle: *mut c_void) -> i32;
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn IsWindowVisible(hwnd: *mut c_void) -> i32;
    }

    static SHARED_HIDDEN_CONSOLE: AtomicBool = AtomicBool::new(false);
    static INIT: Once = Once::new();

    /// Swallow Ctrl-C and Ctrl-Break delivered on a private hidden console so
    /// a child broadcasting to group 0 cannot terminate the parent daemon or
    /// worker. Logoff/shutdown/close events return 0 (unhandled) so OS
    /// shutdown still works normally. Registered as a function pointer rather
    /// than `SetConsoleCtrlHandler(NULL, TRUE)` so children do not inherit
    /// ignored Ctrl-C state in their PEB.
    unsafe extern "system" fn hidden_console_ctrl_handler(ctrl_type: u32) -> i32 {
        i32::from(matches!(ctrl_type, CTRL_C_EVENT | CTRL_BREAK_EVENT))
    }

    pub(super) fn using_shared_hidden_console() -> bool {
        SHARED_HIDDEN_CONSOLE.load(Ordering::Acquire)
    }

    fn console_process_count() -> u32 {
        let mut pid = 0_u32;
        // SAFETY: `pid` is a valid 1-element buffer for `GetConsoleProcessList`.
        unsafe { GetConsoleProcessList(&mut pid, 1) }
    }

    fn console_is_private_hidden() -> bool {
        let mut pids = [0_u32; 2];
        // SAFETY: `pids` is a valid 2-element buffer; a private console freshly
        // created by `CREATE_NO_WINDOW` holds only this process.
        let count = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), 2) };
        count == 1 && pids[0] == std::process::id() && console_window_is_absent_or_hidden()
    }

    fn console_window_is_absent_or_hidden() -> bool {
        // SAFETY: `GetConsoleWindow` and `IsWindowVisible` take no invalid
        // memory and are safe to query for the current process.
        unsafe {
            let hwnd = GetConsoleWindow();
            hwnd.is_null() || IsWindowVisible(hwnd) == 0
        }
    }

    fn resolve_alloc_console_with_options() -> Option<AllocConsoleWithOptionsFn> {
        for dll in ["kernel32.dll\0", "kernelbase.dll\0"] {
            let wide: Vec<u16> = dll.encode_utf16().collect();
            // SAFETY: `wide` is NUL-terminated UTF-16 and the symbol name is
            // NUL-terminated ASCII. Both DLLs remain loaded for process life.
            let proc = unsafe {
                let module = GetModuleHandleW(wide.as_ptr());
                if module.is_null() {
                    std::ptr::null_mut()
                } else {
                    GetProcAddress(module, c"AllocConsoleWithOptions".as_ptr().cast())
                }
            };
            if !proc.is_null() {
                // SAFETY: `AllocConsoleWithOptions` matches the Win32 ABI
                // declared in `consoleapi.h` (Windows 11 24H2+).
                return Some(unsafe {
                    std::mem::transmute::<*mut c_void, AllocConsoleWithOptionsFn>(proc)
                });
            }
        }
        None
    }

    fn alloc_hidden_console_preserving_stdio() -> bool {
        let Some(alloc_fn) = resolve_alloc_console_with_options() else {
            return false;
        };
        // SAFETY: Called single-threaded at startup before worker threads
        // exist. Saving and immediately restoring the three standard handles
        // prevents `AllocConsoleWithOptions` from clobbering redirected log
        // files or pipes.
        unsafe {
            let saved_in = GetStdHandle(STD_INPUT_HANDLE);
            let saved_out = GetStdHandle(STD_OUTPUT_HANDLE);
            let saved_err = GetStdHandle(STD_ERROR_HANDLE);
            let opts = AllocConsoleOptions {
                mode: ALLOC_CONSOLE_MODE_NO_WINDOW,
                use_show_window: 0,
                show_window: 0,
            };
            let mut result = 0_u32;
            let hr = alloc_fn(&opts, &mut result);
            for (kind, handle) in [
                (STD_INPUT_HANDLE, saved_in),
                (STD_OUTPUT_HANDLE, saved_out),
                (STD_ERROR_HANDLE, saved_err),
            ] {
                if handle as isize != -1 {
                    SetStdHandle(kind, handle);
                }
            }
            hr >= 0
                && result == ALLOC_CONSOLE_RESULT_NEW_CONSOLE
                && console_process_count() > 0
                && console_window_is_absent_or_hidden()
        }
    }

    fn install_ctrl_handler() -> bool {
        // SAFETY: `hidden_console_ctrl_handler` has the Win32 `PHANDLER_ROUTINE`
        // signature and lives for the process lifetime.
        unsafe { SetConsoleCtrlHandler(Some(hidden_console_ctrl_handler), 1) != 0 }
    }

    pub(super) fn init_hidden_console() -> bool {
        INIT.call_once(|| {
            let ready = if console_process_count() == 0 {
                alloc_hidden_console_preserving_stdio()
            } else {
                std::env::var_os(HIDDEN_CONSOLE_ENV).as_deref() == Some(OsStr::new("1"))
                    && console_is_private_hidden()
            };
            if ready && install_ctrl_handler() {
                SHARED_HIDDEN_CONSOLE.store(true, Ordering::Release);
            }
        });
        using_shared_hidden_console()
    }
}

/// Ensure a console-less or detached-hidden-console magi process holds one
/// private hidden console for its lifetime so child processes inherit it
/// instead of spawning a new `conhost.exe` per child.
///
/// Must be called early during startup before spawning worker threads so
/// standard handles are preserved race-free if `AllocConsoleWithOptions` runs.
/// A no-op in interactive terminal invocations and off Windows.
pub fn init_hidden_console() -> bool {
    #[cfg(windows)]
    {
        win_console::init_hidden_console()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Whether this process owns a private hidden console shared with its
/// children.
///
/// Always `false` off Windows and for terminal invocations.
#[must_use]
pub fn using_shared_hidden_console() -> bool {
    #[cfg(windows)]
    {
        win_console::using_shared_hidden_console()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Windows `CreateProcessW` creation flags for a quiet child process, combined
/// with `extra` (such as `CREATE_SUSPENDED`).
#[cfg(windows)]
pub(crate) fn child_creation_flags(extra: u32) -> u32 {
    if using_shared_hidden_console() {
        CREATE_NEW_PROCESS_GROUP | extra
    } else {
        CREATE_NO_WINDOW | extra
    }
}

/// Apply the unified quiet creation flags plus `CREATE_SUSPENDED` for a child
/// that will be placed in a Windows Job Object before resuming.
#[cfg(windows)]
pub(crate) fn apply_isolated_child_flags(command: &mut tokio::process::Command) {
    command.creation_flags(child_creation_flags(CREATE_SUSPENDED));
    command.env_remove(HIDDEN_CONSOLE_ENV);
}

/// Configure a detached long-lived magi child (web successor, execution
/// worker, watch supervisor) with its own private hidden console and process
/// group, plus any caller-supplied flags (such as `CREATE_BREAKAWAY_FROM_JOB`).
#[cfg(windows)]
pub(crate) fn configure_detached_hidden_console(
    command: &mut std::process::Command,
    extra_flags: u32,
) {
    use std::os::windows::process::CommandExt as _;
    command.creation_flags(DETACHED_HIDDEN_CONSOLE_FLAGS | extra_flags);
    command.env(HIDDEN_CONSOLE_ENV, "1");
}

/// Spawn without a visible console window.
///
/// Implemented for both `Command` types magi uses - `std` for the few
/// synchronous calls, `tokio` for everything else - so a call site does not
/// have to know which one it is holding, and so no call site has to repeat a
/// `#[cfg(windows)]` block to get it.
///
/// When the process is in shared hidden-console mode (see
/// [`init_hidden_console`]), children inherit that hidden console with
/// `CREATE_NEW_PROCESS_GROUP` instead of spawning a new `conhost.exe`.
/// Otherwise on Windows this passes `CREATE_NO_WINDOW`. A no-op off Windows.
pub trait Quiet {
    /// Apply it, and hand the command back for further building.
    fn quiet(&mut self) -> &mut Self;
}

impl Quiet for std::process::Command {
    fn quiet(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            self.creation_flags(child_creation_flags(0));
            self.env_remove(HIDDEN_CONSOLE_ENV);
        }
        self
    }
}

impl Quiet for tokio::process::Command {
    fn quiet(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            self.creation_flags(child_creation_flags(0));
            self.env_remove(HIDDEN_CONSOLE_ENV);
        }
        self
    }
}

/// Best-effort liveness check for a process id, read through `sysinfo`'s
/// process table - read-only, and no helper process is spawned.
///
/// A pid the table does not list reads as dead. On a platform `sysinfo` does
/// not support the query fails, which reads as alive.
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
    pid_alive_with(pid, platform_pid_alive)
}

/// Apply the conservative policy to one platform liveness query.
///
/// Kept separate from the OS command so queue and daemon tests can exercise
/// dead, live, and unavailable answers without requiring permission to list
/// the machine's processes.
fn pid_alive_with<F>(pid: u32, query: F) -> bool
where
    F: FnOnce(u32) -> std::io::Result<bool>,
{
    match query(pid) {
        Ok(alive) => alive,
        Err(error) => {
            // Sweeping is a poll-loop operation, so state the environment
            // problem at the default log level without repeating it for every
            // protected lock on every poll.
            static REPORTED: std::sync::Once = std::sync::Once::new();
            REPORTED.call_once(|| {
                tracing::warn!(
                    %pid,
                    %error,
                    "process liveness query unavailable; keeping locks rather than treating processes as dead"
                );
            });
            true
        }
    }
}

/// A three-valued liveness read, for a caller that *displays* whether a
/// process is running rather than deciding whether it is safe to reclaim a
/// lock. [`pid_alive`]'s Err-means-alive policy exists to protect a lock a
/// live process still holds — the wrong bias for a report that must never
/// tell an operator a process is confirmed dead just because this build
/// could not ask the platform. `None` here is the honest "could not tell",
/// left for the caller to render as its own "unknown" rather than folded
/// into either `Some` answer.
#[must_use]
pub fn pid_status(pid: u32) -> Option<bool> {
    pid_status_with(pid, platform_pid_alive)
}

/// [`pid_status`] with its process-liveness query supplied by the caller —
/// see [`pid_alive_with`] for why this split exists.
fn pid_status_with<F>(pid: u32, query: F) -> Option<bool>
where
    F: FnOnce(u32) -> std::io::Result<bool>,
{
    query(pid).ok()
}

/// An opaque marker identifying *which* process currently holds `pid`, not
/// merely whether the number is in use — the OS-reported moment it started.
/// A plain integer string (epoch seconds from `sysinfo`), so it does not
/// depend on the locale of the process asking - an `lstart` string recorded
/// under one locale never matched the same process read under another.
/// Compared only for equality by the caller; see [`is_identity_marker`].
///
/// A live pid alone never proves it is the process a caller thinks it is —
/// pids get reused, sometimes within minutes on a busy machine — so
/// [`crate::run::RunState::liveness`] uses this to corroborate a `driver_pid`
/// that answered `pid_status(..) == Some(true)`: it records this marker
/// alongside the pid, and a later mismatch means a *different* process now
/// answers to that number, not that the original one is somehow still
/// running under it. `None` when the platform could not say — a caller must
/// treat that exactly like an unavailable [`pid_status`] query, not as
/// either a match or a mismatch.
#[must_use]
pub fn process_started_at(pid: u32) -> Option<String> {
    platform_process_started_at(pid).ok()
}

/// A per-request memo over [`pid_status`] and [`process_started_at`].
///
/// A listing of hundreds of runs asks about the same few pids over and over,
/// and every ask walks the platform's process table. Asking once per pid is
/// enough within one request; the probe is meant to be dropped with it, never
/// kept, so a stale answer cannot outlive the moment it was read.
pub struct ProcProbe<S, I> {
    status: S,
    identity: I,
    alive: std::collections::HashMap<u32, Option<bool>>,
    started: std::collections::HashMap<u32, Option<String>>,
}

impl ProcProbe<fn(u32) -> Option<bool>, fn(u32) -> Option<String>> {
    /// A probe backed by the real platform queries.
    #[must_use]
    pub fn real() -> Self {
        Self::new(pid_status, process_started_at)
    }
}

impl<S, I> ProcProbe<S, I>
where
    S: FnMut(u32) -> Option<bool>,
    I: FnMut(u32) -> Option<String>,
{
    /// A probe over caller-supplied queries.
    #[must_use]
    pub fn new(status: S, identity: I) -> Self {
        Self {
            status,
            identity,
            alive: std::collections::HashMap::new(),
            started: std::collections::HashMap::new(),
        }
    }

    /// [`pid_status`], asked at most once per pid.
    pub fn status(&mut self, pid: u32) -> Option<bool> {
        *self.alive.entry(pid).or_insert_with(|| (self.status)(pid))
    }

    /// [`process_started_at`], asked at most once per pid.
    pub fn started_at(&mut self, pid: u32) -> Option<String> {
        self.started
            .entry(pid)
            .or_insert_with(|| (self.identity)(pid))
            .clone()
    }
}

/// Ask the platform for one process's start time (epoch seconds), without
/// shelling out to anything.
///
/// `Ok(Some(t))` is a process present in the table, `Ok(None)` is a pid with
/// no process, and `Err` is a platform `sysinfo` does not support or a
/// process table that cannot be read (detected by this process's own absence). Only the
/// requested pid is refreshed, never the whole table. `sysinfo` cannot tell
/// "no such process" from "not visible to this account", so a pid owned by
/// another user that the platform hides reads as absent; identity queries
/// never turn that into a verdict (see [`platform_process_started_at`]).
fn query_process(pid: u32) -> std::io::Result<Option<u64>> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    if !sysinfo::IS_SUPPORTED_SYSTEM {
        return Err(std::io::Error::other(
            "process queries are unavailable on this platform",
        ));
    }
    // Start times are `boot_time + ticks`; when `/proc/stat` has no `btime`
    // sysinfo substitutes a moving clock, so the same live pid would get a
    // different identity on every query.
    #[cfg(target_os = "linux")]
    if !linux_boot_time_readable() {
        return Err(std::io::Error::other(
            "boot time is unreadable: start times would not be stable",
        ));
    }
    let pid = Pid::from_u32(pid);
    let own = Pid::from_u32(std::process::id());
    let mut system = System::new();
    system.refresh_processes_specifics(
        // A duplicate pid in the list (querying this process itself) makes
        // `sysinfo` drop the entry, so the target is only added when distinct.
        ProcessesToUpdate::Some(&if pid == own {
            vec![own]
        } else {
            vec![pid, own]
        }),
        true,
        ProcessRefreshKind::nothing(),
    );
    // A failed refresh (an unmounted or unreadable process table) looks
    // exactly like an absent pid. This process is certainly alive, so if it
    // is missing too the table cannot be trusted and nothing may be read
    // from it - least of all "dead".
    if system.process(own).is_none_or(|p| p.start_time() == 0) {
        return Err(std::io::Error::other(
            "process table is unreadable: this process is not listed",
        ));
    }
    let found = system.process(pid).map(sysinfo::Process::start_time);
    // This process being listed does not prove the target's own entry could
    // be read: an unreadable `/proc/<pid>/stat` leaves a live pid out of the
    // table. On Linux the directory itself is the independent witness - if it
    // exists the pid is alive, so "not listed" is a failed read, not absence.
    #[cfg(target_os = "linux")]
    if found.is_none() && std::path::Path::new(&format!("/proc/{}", pid.as_u32())).exists() {
        return Err(std::io::Error::other(
            "process exists but its entry could not be read",
        ));
    }
    // Other unixes have no `/proc` to consult, but `kill(pid, 0)` is an
    // independent witness: anything but `ESRCH` means the pid exists, so a
    // target `sysinfo` could not read (e.g. a denied `KERN_PROCARGS2` on
    // macOS) is an unreadable entry, not an absent process.
    #[cfg(all(unix, not(target_os = "linux")))]
    if found.is_none() && unix_pid_exists(pid.as_u32()) {
        return Err(std::io::Error::other(
            "process exists but its entry could not be read",
        ));
    }
    // A `/proc` mounted with `hidepid=1|2` hides other users' pids, so a miss
    // is not proof of absence there; nor is one when the mount table cannot
    // be read to tell.
    #[cfg(target_os = "linux")]
    if found.is_none() && !linux_proc_shows_all_pids() {
        return Err(std::io::Error::other(
            "absence is unprovable: /proc may hide other users' processes",
        ));
    }
    Ok(found)
}

/// Whether `kill(pid, 0)` finds the pid: success or `EPERM` both mean it exists.
#[cfg(all(unix, not(target_os = "linux")))]
fn unix_pid_exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks for existence and delivers nothing.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Whether `/proc/stat` carries a non-zero `btime` line.
#[cfg(target_os = "linux")]
fn linux_boot_time_readable() -> bool {
    std::fs::read_to_string("/proc/stat").is_ok_and(|s| stat_has_btime(&s))
}

#[cfg(any(target_os = "linux", test))]
fn stat_has_btime(stat: &str) -> bool {
    stat.lines().any(|l| {
        l.strip_prefix("btime ")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .is_some_and(|v| v > 0)
    })
}

/// Whether `/proc` is mounted without `hidepid`, judged from the mount table.
/// An unreadable table or a missing `/proc` entry answers `false`.
#[cfg(target_os = "linux")]
fn linux_proc_shows_all_pids() -> bool {
    std::fs::read_to_string("/proc/self/mountinfo").is_ok_and(|s| mountinfo_proc_unhidden(&s))
}

#[cfg(any(target_os = "linux", test))]
fn mountinfo_proc_unhidden(mountinfo: &str) -> bool {
    mountinfo
        .lines()
        .rfind(|l| l.split_whitespace().nth(4) == Some("/proc"))
        .is_some_and(|l| {
            // Only an explicit `hidepid=0` / `off` (or no option at all)
            // shows every pid; `1`, `2`, `4`, their names and any value not
            // known here all count as restricted.
            l.split(|c: char| c.is_whitespace() || c == ',')
                .filter_map(|o| o.strip_prefix("hidepid="))
                .all(|v| matches!(v, "0" | "off"))
        })
}

/// Whether the identity marker format is the current one: a plain integer
/// (epoch seconds). Runs recorded before this format carried the locale
/// dependent `ps -o lstart=` text, which can never be compared reliably.
#[must_use]
pub fn is_identity_marker(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// The start time as a locale-independent integer string. Absence, a zero
/// (what `sysinfo` reports when the platform would not say) and an
/// unsupported platform are all errors, so a comparison can never be built
/// on a guess.
fn platform_process_started_at(pid: u32) -> std::io::Result<String> {
    match query_process(pid)? {
        Some(0) => Err(std::io::Error::other("process start time is unavailable")),
        Some(started) => Ok(started.to_string()),
        None => Err(std::io::Error::other("no such process")),
    }
}

fn platform_pid_alive(pid: u32) -> std::io::Result<bool> {
    query_process(pid).map(|found| found.is_some())
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
        assert_eq!(
            DETACHED_HIDDEN_CONSOLE_FLAGS,
            CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        );
    }

    #[cfg(windows)]
    mod windows_hidden_console_tests {
        use std::ffi::c_void;
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
        use std::os::windows::process::CommandExt as _;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        use super::*;

        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CTRL_C_EVENT: u32 = 0;
        const CTRL_BREAK_EVENT: u32 = 1;
        const HELPER_ENV: &str = "MAGI_PROC_HIDDEN_CONSOLE_TEST_MODE";

        #[repr(C)]
        #[derive(Default)]
        struct Accounting {
            times: [i64; 4],
            faults: u32,
            total: u32,
            active: u32,
            terminated: u32,
        }

        #[repr(C)]
        #[derive(Default)]
        struct ThreadEntry {
            size: u32,
            usage: u32,
            id: u32,
            owner: u32,
            base_priority: i32,
            delta_priority: i32,
            flags: u32,
        }

        #[repr(C)]
        struct ProcessEntryW {
            size: u32,
            cnt_usage: u32,
            pid: u32,
            default_heap_id: usize,
            module_id: u32,
            cnt_threads: u32,
            parent_pid: u32,
            pri_class_base: i32,
            flags: u32,
            exe_file: [u16; 260],
        }

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> *mut c_void;
            fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
            fn QueryInformationJobObject(
                job: *mut c_void,
                class: i32,
                info: *mut c_void,
                size: u32,
                returned: *mut u32,
            ) -> i32;
            fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
            fn Thread32First(snapshot: *mut c_void, entry: *mut ThreadEntry) -> i32;
            fn Thread32Next(snapshot: *mut c_void, entry: *mut ThreadEntry) -> i32;
            fn Process32FirstW(snapshot: *mut c_void, entry: *mut ProcessEntryW) -> i32;
            fn Process32NextW(snapshot: *mut c_void, entry: *mut ProcessEntryW) -> i32;
            fn OpenThread(access: u32, inherit: i32, id: u32) -> *mut c_void;
            fn ResumeThread(thread: *mut c_void) -> u32;
            fn GetConsoleWindow() -> *mut c_void;
            fn GetModuleHandleW(module_name: *const u16) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, proc_name: *const u8) -> *mut c_void;
            fn GenerateConsoleCtrlEvent(ctrl_event: u32, process_group_id: u32) -> i32;
        }

        #[link(name = "user32")]
        unsafe extern "system" {
            fn IsWindowVisible(hwnd: *mut c_void) -> i32;
            fn EnumWindows(
                cb: unsafe extern "system" fn(*mut c_void, isize) -> i32,
                lparam: isize,
            ) -> i32;
            fn GetWindowThreadProcessId(hwnd: *mut c_void, pid: *mut u32) -> u32;
        }

        fn alloc_console_with_options_available() -> bool {
            for dll in ["kernel32.dll\0", "kernelbase.dll\0"] {
                let wide: Vec<u16> = dll.encode_utf16().collect();
                let proc = unsafe {
                    let module = GetModuleHandleW(wide.as_ptr());
                    if module.is_null() {
                        std::ptr::null_mut()
                    } else {
                        GetProcAddress(module, c"AllocConsoleWithOptions".as_ptr().cast())
                    }
                };
                if !proc.is_null() {
                    return true;
                }
            }
            false
        }

        fn resume_primary_thread(pid: u32) {
            unsafe {
                let snap = CreateToolhelp32Snapshot(4, 0);
                assert_ne!(snap as isize, -1, "CreateToolhelp32Snapshot failed");
                let snap = OwnedHandle::from_raw_handle(snap);
                let mut entry = ThreadEntry {
                    size: std::mem::size_of::<ThreadEntry>() as u32,
                    ..ThreadEntry::default()
                };
                let mut ok = Thread32First(snap.as_raw_handle(), &mut entry);
                while ok != 0 {
                    if entry.owner == pid {
                        let th = OpenThread(2, 0, entry.id);
                        assert!(!th.is_null(), "OpenThread failed");
                        let th = OwnedHandle::from_raw_handle(th);
                        assert_ne!(ResumeThread(th.as_raw_handle()), u32::MAX);
                        return;
                    }
                    entry.size = std::mem::size_of::<ThreadEntry>() as u32;
                    ok = Thread32Next(snap.as_raw_handle(), &mut entry);
                }
                panic!("primary thread not found for pid {pid}");
            }
        }

        fn count_conhost_children(parent_pid: u32) -> usize {
            unsafe {
                let snap = CreateToolhelp32Snapshot(0x0000_0002, 0);
                assert_ne!(snap as isize, -1);
                let snap = OwnedHandle::from_raw_handle(snap);
                let mut entry: ProcessEntryW = std::mem::zeroed();
                entry.size = std::mem::size_of::<ProcessEntryW>() as u32;
                let mut count = 0;
                let mut ok = Process32FirstW(snap.as_raw_handle(), &mut entry);
                while ok != 0 {
                    if entry.parent_pid == parent_pid {
                        let end = entry
                            .exe_file
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(entry.exe_file.len());
                        if String::from_utf16_lossy(&entry.exe_file[..end])
                            .eq_ignore_ascii_case("conhost.exe")
                        {
                            count += 1;
                        }
                    }
                    entry.size = std::mem::size_of::<ProcessEntryW>() as u32;
                    ok = Process32NextW(snap.as_raw_handle(), &mut entry);
                }
                count
            }
        }

        fn any_visible_window_for_pids(pids: &[u32]) -> bool {
            struct Ctx<'a> {
                pids: &'a [u32],
                found: bool,
            }
            unsafe extern "system" fn cb(hwnd: *mut c_void, lparam: isize) -> i32 {
                let ctx = unsafe { &mut *(lparam as *mut Ctx<'_>) };
                let mut pid = 0_u32;
                unsafe {
                    GetWindowThreadProcessId(hwnd, &mut pid);
                }
                if ctx.pids.contains(&pid) && unsafe { IsWindowVisible(hwnd) != 0 } {
                    ctx.found = true;
                    return 0;
                }
                1
            }
            let mut ctx = Ctx { pids, found: false };
            unsafe {
                EnumWindows(cb, std::ptr::from_mut(&mut ctx) as isize);
            }
            ctx.found
        }

        fn console_window_visible() -> bool {
            unsafe {
                let hwnd = GetConsoleWindow();
                !hwnd.is_null() && IsWindowVisible(hwnd) != 0
            }
        }

        fn run_helper_in_job(
            mode: &str,
            extra_envs: &[(&str, &str)],
            configure_cmd: impl FnOnce(&mut Command),
        ) -> (u32, std::process::Output) {
            let exe = std::env::current_exe().expect("test executable");
            let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
            assert!(!job.is_null());
            let job = unsafe { OwnedHandle::from_raw_handle(job) };
            let mut cmd = Command::new(exe);
            cmd.args([
                "--exact",
                "proc::tests::windows_hidden_console_tests::hidden_console_test_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_ENV, mode)
            .env_remove(HIDDEN_CONSOLE_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
            for (k, v) in extra_envs {
                cmd.env(k, v);
            }
            configure_cmd(&mut cmd);
            let child = cmd.spawn().expect("spawn helper");
            let pid = child.id();
            assert_ne!(
                unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) },
                0,
                "AssignProcessToJobObject failed"
            );
            resume_primary_thread(pid);
            let output = child.wait_with_output().expect("wait helper");
            let mut info = Accounting::default();
            let mut returned = 0_u32;
            assert_ne!(
                unsafe {
                    QueryInformationJobObject(
                        job.as_raw_handle(),
                        1,
                        std::ptr::from_mut(&mut info).cast(),
                        std::mem::size_of::<Accounting>() as u32,
                        &mut returned,
                    )
                },
                0
            );
            (info.total, output)
        }

        fn helper_child_command(mode: &str) -> Command {
            let mut cmd = Command::new(std::env::current_exe().expect("test executable"));
            cmd.args([
                "--exact",
                "proc::tests::windows_hidden_console_tests::hidden_console_test_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_ENV, mode)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
            cmd
        }

        #[test]
        fn hidden_console_test_helper() {
            let Ok(mode) = std::env::var(HELPER_ENV) else {
                return;
            };
            match mode.as_str() {
                "noop" => {
                    println!("CHILD_NOOP_OK");
                }
                "sleep" => {
                    let ms: u64 = std::env::var("MAGI_PROC_TEST_SLEEP_MS")
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(500);
                    println!(
                        "SLEEP_PID={} VIS={}",
                        std::process::id(),
                        console_window_visible()
                    );
                    std::thread::sleep(Duration::from_millis(ms));
                }
                "grandchild-check" => {
                    assert!(
                        !console_window_visible(),
                        "child console window became visible"
                    );
                    assert!(
                        std::env::var_os(HIDDEN_CONSOLE_ENV).is_none(),
                        "MAGI_HIDDEN_CONSOLE leaked to quiet child"
                    );
                    // Spawn a grandchild with NO creation_flags (mirroring git
                    // helpers like git-remote-https or sh hooks).
                    let gc = helper_child_command("sleep")
                        .env("MAGI_PROC_TEST_SLEEP_MS", "350")
                        .spawn()
                        .expect("spawn grandchild");
                    let gc_pid = gc.id();
                    std::thread::sleep(Duration::from_millis(150));
                    let gc_conhosts = count_conhost_children(gc_pid);
                    let gc_vis = any_visible_window_for_pids(&[std::process::id(), gc_pid]);
                    let out = gc.wait_with_output().expect("wait grandchild");
                    assert!(out.status.success());
                    assert_eq!(gc_conhosts, 0, "grandchild spawned conhost.exe");
                    assert!(!gc_vis, "grandchild opened a visible window");
                    println!("GRANDCHILD_OK");
                }
                "parent-batch" => {
                    let shared = init_hidden_console();
                    let k: usize = std::env::var("MAGI_PROC_TEST_K").unwrap().parse().unwrap();
                    let expect_shared =
                        std::env::var("MAGI_PROC_TEST_EXPECT_SHARED").unwrap() == "1";
                    assert_eq!(shared, expect_shared, "shared hidden-console mode mismatch");
                    assert_eq!(using_shared_hidden_console(), expect_shared);
                    assert!(
                        !console_window_visible(),
                        "parent console window became visible"
                    );
                    for _ in 0..k {
                        let mut cmd = helper_child_command("noop");
                        cmd.quiet();
                        let out = cmd.output().expect("spawn quiet child");
                        assert!(out.status.success());
                        assert!(
                            String::from_utf8_lossy(&out.stdout).contains("CHILD_NOOP_OK"),
                            "stdout lost from quiet child"
                        );
                    }
                    // Also check a child + grandchild pair for zero visible windows
                    // and zero extra conhost.exe when K > 0.
                    if k > 0 {
                        let mut cmd = helper_child_command("grandchild-check");
                        cmd.quiet();
                        let child = cmd.spawn().expect("spawn grandchild-check");
                        let child_pid = child.id();
                        std::thread::sleep(Duration::from_millis(150));
                        let child_conhosts = count_conhost_children(child_pid);
                        let parent_vis =
                            any_visible_window_for_pids(&[std::process::id(), child_pid]);
                        let out = child.wait_with_output().expect("wait grandchild-check");
                        assert!(
                            out.status.success(),
                            "grandchild-check failed: {}",
                            String::from_utf8_lossy(&out.stderr)
                        );
                        assert!(
                            String::from_utf8_lossy(&out.stdout).contains("GRANDCHILD_OK"),
                            "missing GRANDCHILD_OK"
                        );
                        assert!(!parent_vis, "visible window detected for parent or child");
                        if expect_shared {
                            assert_eq!(
                                count_conhost_children(std::process::id()),
                                1,
                                "parent must own exactly one conhost.exe"
                            );
                            assert_eq!(
                                child_conhosts, 0,
                                "child must inherit hidden console without spawning conhost.exe"
                            );
                        }
                    }
                    println!("PARENT_STDOUT_OK");
                    eprintln!("PARENT_STDERR_OK");
                }
                "parent-signals" => {
                    assert!(init_hidden_console());
                    let mut sib_a_cmd = helper_child_command("sleep");
                    sib_a_cmd.quiet().env("MAGI_PROC_TEST_SLEEP_MS", "3000");
                    let mut sib_a = sib_a_cmd.spawn().expect("spawn sibling A");
                    let a_pid = sib_a.id();

                    let mut sib_b_cmd = helper_child_command("sleep");
                    sib_b_cmd.quiet().env("MAGI_PROC_TEST_SLEEP_MS", "700");
                    let mut sib_b = sib_b_cmd.spawn().expect("spawn sibling B");

                    std::thread::sleep(Duration::from_millis(200));
                    // Targeted CTRL_BREAK to A's own process group only.
                    assert_ne!(
                        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, a_pid) },
                        0
                    );
                    // Broadcast CTRL_C to group 0: parent handler swallows it and
                    // sibling B ignores it via CREATE_NEW_PROCESS_GROUP.
                    assert_ne!(unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) }, 0);

                    let a_status = sib_a.wait().expect("wait sibling A");
                    let b_status = sib_b.wait().expect("wait sibling B");
                    assert!(
                        !a_status.success(),
                        "targeted CTRL_BREAK must terminate sibling A"
                    );
                    assert!(
                        b_status.success(),
                        "sibling B must survive sibling A's CTRL_BREAK and broadcast CTRL_C"
                    );

                    // Job Object containment termination on child C must not affect
                    // the parent's shared hidden console or later children.
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("tokio runtime");
                    runtime.block_on(async {
                        let mut c_cmd = tokio::process::Command::new(
                            std::env::current_exe().expect("test executable"),
                        );
                        c_cmd
                            .args([
                                "--exact",
                                "proc::tests::windows_hidden_console_tests::hidden_console_test_helper",
                                "--nocapture",
                                "--test-threads=1",
                            ])
                            .env(HELPER_ENV, "sleep")
                            .env("MAGI_PROC_TEST_SLEEP_MS", "5000")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null());
                        apply_isolated_child_flags(&mut c_cmd);
                        let mut c = c_cmd.spawn().expect("spawn isolated child C");
                        let c_pid = c.id().expect("child C pid");
                        let job = crate::chat_watch::windows::Containment::assign_handle(
                            c.raw_handle().expect("child C handle"),
                        )
                        .expect("assign job");
                        job.resume_suspended(c_pid).expect("resume child C");
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        job.stop_and_wait();
                        let _ = c.wait().await;
                    });

                    // A subsequent child D still inherits the intact hidden console.
                    let mut d_cmd = helper_child_command("noop");
                    d_cmd.quiet();
                    let d_out = d_cmd.output().expect("spawn child D");
                    assert!(d_out.status.success());
                    assert!(String::from_utf8_lossy(&d_out.stdout).contains("CHILD_NOOP_OK"));
                    println!("SIGNALS_AND_JOB_OK");
                }
                "terminal-marker-ignored" => {
                    assert!(
                        !init_hidden_console(),
                        "inherited console with multiple processes must never enter hidden-console mode"
                    );
                    assert!(!using_shared_hidden_console());
                    assert_eq!(child_creation_flags(0), CREATE_NO_WINDOW);
                    println!("TERMINAL_MODE_OK");
                }
                other => panic!("unknown helper mode: {other}"),
            }
        }

        #[test]
        fn console_less_parent_shares_one_hidden_console_across_children_with_no_visible_windows() {
            // Mode 1: Detached magi process spawned with our hidden-console flags
            // (works on every Windows version, including Server 2022 and Win11).
            let (t0, out0) = run_helper_in_job(
                "parent-batch",
                &[
                    ("MAGI_PROC_TEST_K", "0"),
                    ("MAGI_PROC_TEST_EXPECT_SHARED", "1"),
                ],
                |cmd| {
                    configure_detached_hidden_console(cmd, CREATE_SUSPENDED);
                },
            );
            assert!(
                out0.status.success(),
                "mode1 k=0 failed: {}",
                String::from_utf8_lossy(&out0.stderr)
            );
            let (t5, out5) = run_helper_in_job(
                "parent-batch",
                &[
                    ("MAGI_PROC_TEST_K", "5"),
                    ("MAGI_PROC_TEST_EXPECT_SHARED", "1"),
                ],
                |cmd| {
                    configure_detached_hidden_console(cmd, CREATE_SUSPENDED);
                },
            );
            assert!(
                out5.status.success(),
                "mode1 k=5 failed: {}",
                String::from_utf8_lossy(&out5.stderr)
            );
            assert!(String::from_utf8_lossy(&out5.stdout).contains("PARENT_STDOUT_OK"));
            assert!(String::from_utf8_lossy(&out5.stderr).contains("PARENT_STDERR_OK"));
            // K=0 has parent + 1 conhost (=2). K=5 adds 5 noop children + 1 child +
            // 1 grandchild (=7 processes) with 0 extra conhost.exe.
            assert_eq!(
                t0, 2,
                "expected parent + 1 lifetime conhost.exe at K=0, got {t0}"
            );
            assert_eq!(
                t5 - t0,
                7,
                "expected exactly 7 child/grandchild processes and 0 extra conhost.exe, got {}",
                t5 - t0
            );

            // Mode 2: Externally launched console-less process (DETACHED_PROCESS).
            let expect_alloc = alloc_console_with_options_available();
            let expect_flag = if expect_alloc { "1" } else { "0" };
            let (d0, dout0) = run_helper_in_job(
                "parent-batch",
                &[
                    ("MAGI_PROC_TEST_K", "0"),
                    ("MAGI_PROC_TEST_EXPECT_SHARED", expect_flag),
                ],
                |cmd| {
                    cmd.creation_flags(DETACHED_PROCESS | CREATE_SUSPENDED);
                },
            );
            assert!(
                dout0.status.success(),
                "mode2 k=0 failed: {}",
                String::from_utf8_lossy(&dout0.stderr)
            );
            let (d5, dout5) = run_helper_in_job(
                "parent-batch",
                &[
                    ("MAGI_PROC_TEST_K", "5"),
                    ("MAGI_PROC_TEST_EXPECT_SHARED", expect_flag),
                ],
                |cmd| {
                    cmd.creation_flags(DETACHED_PROCESS | CREATE_SUSPENDED);
                },
            );
            assert!(
                dout5.status.success(),
                "mode2 k=5 failed: {}",
                String::from_utf8_lossy(&dout5.stderr)
            );
            assert!(String::from_utf8_lossy(&dout5.stdout).contains("PARENT_STDOUT_OK"));
            assert!(String::from_utf8_lossy(&dout5.stderr).contains("PARENT_STDERR_OK"));
            if expect_alloc {
                assert_eq!(
                    d0, 2,
                    "expected detached parent + 1 AllocConsoleWithOptions conhost.exe at K=0, got {d0}"
                );
                assert_eq!(
                    d5 - d0,
                    7,
                    "expected 0 extra conhost.exe across 5 children + 1 child/grandchild pair, got {}",
                    d5 - d0
                );
            }
        }

        #[test]
        fn shared_hidden_console_isolates_signals_and_job_termination_between_siblings() {
            let (_, out) = run_helper_in_job("parent-signals", &[], |cmd| {
                configure_detached_hidden_console(cmd, CREATE_SUSPENDED);
            });
            assert!(
                out.status.success(),
                "signal isolation helper failed:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(String::from_utf8_lossy(&out.stdout).contains("SIGNALS_AND_JOB_OK"));
        }

        #[test]
        fn terminal_parent_keeps_create_no_window_and_ignores_inherited_marker() {
            // Even if MAGI_HIDDEN_CONSOLE=1 is present, a child sharing the test
            // runner's console (process count >= 2) must stay in terminal mode.
            let (_, out) = run_helper_in_job(
                "terminal-marker-ignored",
                &[(HIDDEN_CONSOLE_ENV, "1")],
                |cmd| {
                    cmd.creation_flags(CREATE_SUSPENDED);
                },
            );
            assert!(
                out.status.success(),
                "terminal helper failed:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(String::from_utf8_lossy(&out.stdout).contains("TERMINAL_MODE_OK"));
        }
    }

    #[test]
    fn btime_and_hidepid_parsers_distrust_what_they_cannot_confirm() {
        assert!(stat_has_btime("cpu 1 2\nbtime 1700000000\n"));
        assert!(!stat_has_btime("cpu 1 2\n"));
        assert!(!stat_has_btime("btime 0\n"));
        let open = "25 1 0:5 / /proc rw,nosuid - proc proc rw";
        let hidden = "25 1 0:5 / /proc rw,nosuid - proc proc rw,hidepid=2";
        assert!(mountinfo_proc_unhidden(open));
        assert!(!mountinfo_proc_unhidden(hidden));
        for v in ["1", "4", "ptraceable", "noaccess", "future"] {
            let line = format!("25 1 0:5 / /proc rw - proc proc rw,hidepid={v}");
            assert!(!mountinfo_proc_unhidden(&line), "hidepid={v}");
        }
        assert!(mountinfo_proc_unhidden(
            "25 1 0:5 / /proc rw - proc proc rw,hidepid=0"
        ));
        assert!(!mountinfo_proc_unhidden(""));
    }

    #[test]
    fn pid_liveness_policy_is_deterministic_without_an_os_process_query() {
        assert!(pid_alive_with(42, |_| Ok(true)));
        assert!(!pid_alive_with(42, |_| Ok(false)));
    }

    #[test]
    fn an_unavailable_process_query_is_never_mistaken_for_a_dead_process() {
        assert!(pid_alive_with(42, |_| Err(std::io::Error::other(
            "access denied"
        ))));
    }

    /// Unlike [`pid_alive_with`]'s Err-means-alive bias, the three-valued read
    /// leaves an unavailable query as `None` rather than inventing either
    /// answer — a display that guessed "dead" here would be exactly the wrong
    /// kind of confidence this exists to avoid.
    #[test]
    fn pid_status_reports_alive_dead_and_unknown_as_three_distinct_answers() {
        assert_eq!(pid_status_with(42, |_| Ok(true)), Some(true));
        assert_eq!(pid_status_with(42, |_| Ok(false)), Some(false));
        assert_eq!(
            pid_status_with(42, |_| Err(std::io::Error::other("access denied"))),
            None
        );
    }

    /// このテスト自身の PID を OS に問い合わせるスモーク診断。
    ///
    /// CI では実際のコマンド実行と成功出力の解析を必須にする。制限された
    /// 対話席で問い合わせ自体が使えない場合は、その事実を出力して成功結果や
    /// 死んだプロセスと取り違えない。実行中の PID を dead と報告した場合と、
    /// CI で問い合わせが利用不能な場合は失敗にする。
    #[test]
    fn platform_query_reports_this_running_process_as_alive_or_unavailable() {
        let pid = std::process::id();
        match platform_pid_alive(pid) {
            Ok(true) => {}
            Ok(false) => {
                panic!("OS の PID 問い合わせが実行中のテストプロセス {pid} を dead と報告した")
            }
            Err(error) if std::env::var_os("CI").is_some() => {
                panic!("CI で OS の PID 問い合わせを実行できない（テストプロセス {pid}）: {error}")
            }
            Err(error) => {
                eprintln!("OS の PID 問い合わせは利用できません（テストプロセス {pid}）: {error}")
            }
        }
    }

    /// 同じスモーク診断を `process_started_at` にも適用する: 実行中の
    /// このテストプロセス自身に対して呼ぶと、利用可能な環境では必ず何か
    /// 返り、そして二回呼んでも同じ値を返す — 同一プロセスの起動時刻が
    /// 問い合わせのたびにずれては、pid 再利用との判別に使えない。
    #[test]
    fn platform_query_reports_this_running_process_start_time_consistently_or_unavailable() {
        let pid = std::process::id();
        match (
            platform_process_started_at(pid),
            platform_process_started_at(pid),
        ) {
            (Ok(first), Ok(second)) => {
                assert_eq!(
                    first, second,
                    "同一の生存プロセスへの二回の問い合わせが食い違った"
                );
                assert!(is_identity_marker(&first), "整数文字列でない: {first}");
            }
            (Err(error), _) | (_, Err(error)) if std::env::var_os("CI").is_some() => {
                panic!("CI で起動時刻の問い合わせを実行できない（テストプロセス {pid}）: {error}")
            }
            _ => eprintln!("起動時刻の問い合わせは利用できません（テストプロセス {pid}）"),
        }
    }
}
