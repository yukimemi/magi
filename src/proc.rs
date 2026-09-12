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

fn platform_pid_alive(pid: u32) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        match std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
        {
            Ok(o) => Ok(parse_unix_kill_output(o.status.success(), &o.stderr)),
            Err(error) => Err(error),
        }
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .quiet()
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output();
        match out {
            Ok(o) => tasklist_result(
                pid,
                o.status.success(),
                &o.stdout,
                &o.stderr,
                &o.status.to_string(),
            ),
            Err(error) => Err(error),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Ok(true)
    }
}

/// 数値の PID から推測せず、`kill -0` の終了状態と診断を解釈する。
/// 明示的な "no such process" 診断だけを死亡の証拠とする。
#[cfg(any(unix, test))]
fn parse_unix_kill_output(success: bool, stderr: &[u8]) -> bool {
    if success {
        return true;
    }
    !String::from_utf8_lossy(stderr)
        .to_lowercase()
        .contains("no such process")
}

/// `tasklist /FO CSV` の出力を解釈する。一致しない場合、要求した PID の
/// フィールドを持つ行は存在しない。
#[cfg(any(windows, test))]
fn parse_windows_tasklist_output(pid: u32, stdout: &[u8]) -> bool {
    let expected = pid.to_string();
    String::from_utf8_lossy(stdout).lines().any(|line| {
        tasklist_csv_fields(line)
            .is_some_and(|fields| fields.get(1).is_some_and(|field| field == &expected))
    })
}

/// `tasklist` が出す、二重引用符と `""` エスケープを持つ CSV の一行を分ける。
/// 壊れた CSV は PID 不一致として扱う。コマンド失敗の利用不能判定は呼び出し側が
/// 保持する。
#[cfg(any(windows, test))]
fn tasklist_csv_fields(line: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            _ => field.push(ch),
        }
    }
    (!quoted).then(|| {
        fields.push(field);
        fields
    })
}

/// `tasklist` の失敗を、利用不能な問い合わせとして保持する。
#[cfg(any(windows, test))]
fn tasklist_result(
    pid: u32,
    success: bool,
    stdout: &[u8],
    stderr: &[u8],
    status: &str,
) -> std::io::Result<bool> {
    if success {
        Ok(parse_windows_tasklist_output(pid, stdout))
    } else {
        Err(std::io::Error::other(format!(
            "tasklist exited {status}: {}",
            String::from_utf8_lossy(stderr).trim()
        )))
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

    /// 本番パーサー用のコマンド出力フィクスチャであり、特定 PID の OS 上の
    /// 死亡状態を主張するものではない。
    #[test]
    fn unix_kill_output_only_marks_no_such_process_as_dead() {
        assert!(parse_unix_kill_output(true, b""));
        assert!(!parse_unix_kill_output(
            false,
            b"kill: (12345) - No such process\n"
        ));
        assert!(parse_unix_kill_output(
            false,
            b"kill: (12345) - Operation not permitted\n"
        ));
    }

    /// 本番パーサー用のコマンド出力フィクスチャであり、OS の生存照会ではない。
    /// 失敗した `tasklist` は死亡ではなく利用不能のままとする。
    #[test]
    fn windows_tasklist_csv_parsing_handles_match_no_match_and_error() {
        let pid = 12345;
        assert!(parse_windows_tasklist_output(
            pid,
            b"\"magi.exe\",\"12345\",\"Console\",\"1\",\"10 K\"\r\n"
        ));
        assert!(parse_windows_tasklist_output(
            pid,
            b"\"magi,worker.exe\",\"12345\",\"Console\",\"1\",\"10 K\"\r\n"
        ));
        assert!(!parse_windows_tasklist_output(
            pid,
            b"INFO: No tasks are running which match the specified criteria.\r\n"
        ));
        assert!(
            tasklist_result(
                pid,
                true,
                b"\"magi.exe\",\"12345\",\"Console\",\"1\",\"10 K\"\r\n",
                b"",
                "exit status: 0",
            )
            .expect("CSV の一致行は生存を示す")
        );

        let error = tasklist_result(pid, false, b"", b"Access is denied.\r\n", "exit status: 1")
            .expect_err("tasklist の失敗は死亡ではなく利用不能である");
        assert!(error.to_string().contains("Access is denied."));
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
}
