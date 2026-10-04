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
        ProcessesToUpdate::Some(&[pid, own]),
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

    /// Every `Command::new` in this crate's own sources is either quieted or
    /// carries one of the two exemptions this module's doc explains.
    ///
    /// A textual scan, not a lint: nothing in `cargo clippy` knows that a
    /// console-app child of a console-less parent gets a window, so nothing
    /// catches a spawn that forgot `.quiet()` short of a human reading every
    /// call site - which is exactly how `disk.rs`'s PowerShell probe and
    /// `graph.rs`'s `gh pr create` went unquieted despite every neighbouring
    /// spawn getting it right. Each `Command::new` is checked against the
    /// text between it and the next one in the same file (or end of file),
    /// which is always enough to cover its own builder chain and never
    /// bleeds into an unrelated spawn's exemption.
    #[test]
    fn every_spawn_in_the_crate_is_quiet_or_documented_as_exempt() {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&src_dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_owned();
            if file_name == "tui.rs" {
                // explorer / open / xdg-open: GUI launchers, not console
                // children - out of scope by design (see AGENTS.md).
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source file");
            let lines: Vec<&str> = text.lines().collect();
            let spawn_at: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.contains("Command::new("))
                .map(|(i, _)| i)
                .collect();
            for (pos, &start) in spawn_at.iter().enumerate() {
                let end = spawn_at.get(pos + 1).copied().unwrap_or(lines.len());
                let block = lines[start..end].join("\n");
                if block.contains(".quiet()") {
                    continue;
                }
                // `spawn_successor`'s DETACHED_PROCESS successor has no
                // console to inherit in the first place; see its doc comment
                // in `web.rs`.
                if block.contains("DETACHED_PROCESS") {
                    continue;
                }
                // A spawn guarded by `#[cfg(unix)]` a few lines above cannot
                // hit the Windows console bug at all.
                let preceding = lines[start.saturating_sub(5)..start].join("\n");
                if preceding.contains("#[cfg(unix)]") {
                    continue;
                }
                offenders.push(format!("{file_name}:{}", start + 1));
            }
        }
        assert!(
            offenders.is_empty(),
            "Command::new without .quiet() and no documented exemption: {offenders:?}"
        );
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
