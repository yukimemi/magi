//! Real shared-directory and control-plane fixtures. No operator state is used.
mod common;

#[cfg(unix)]
fn coordination() -> &'static magi::config::Coordination {
    static POLICY: std::sync::LazyLock<magi::config::Coordination> = std::sync::LazyLock::new(
        || {
            toml::from_str("contract_file = 'updater.contract'\ncontract_token = 'test-protocol-v1'\nlock_file = 'updater.lock'").unwrap()
        },
    );
    &POLICY
}

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use magi::agent::worktree_writable_roots;
use magi::ask::Questions;

fn checked(cmd: &mut Command) -> Output {
    let out = cmd.output().expect("start fixture command");
    assert!(out.status.success(), "{cmd:?}: {out:?}");
    out
}

/// Run outside an enclosing seat sandbox. This records administration-path
/// behavior rather than assuming that a common-directory grant overrides it.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires the installed sandbox and permission to launch sandbox-exec"]
async fn sandbox_git_operations_and_network_policy() {
    let fx = Fixture::new();
    let roots = worktree_writable_roots(&fx.seat, &fx.home).await.unwrap();
    let unrelated = fx.home.join("queue");
    std::fs::create_dir(&unrelated).unwrap();
    checked(Command::new("git").current_dir(&fx.seat).args([
        "remote",
        "add",
        "origin",
        fx.primary.to_str().unwrap(),
    ]));
    let baseline = checked(Command::new("/usr/bin/python3").args([
        "-c",
        "import socket; print(socket.getaddrinfo('github.com', 443))",
    ]));
    println!(
        "host DNS succeeded: {}",
        String::from_utf8_lossy(&baseline.stdout)
    );
    let script = r#"
import json, os, pathlib, socket, subprocess
pathlib.Path('probe').write_text('sandbox edit\n')
for args in [['status', '--short'], ['diff'], ['log', '-1', '--oneline'],
             ['add', 'probe'], ['-c', 'user.name=Fixture', '-c',
              'user.email=fixture@example.invalid', 'commit', '-m', 'probe'],
             ['fetch', 'origin', 'main']]:
    p = subprocess.run(['git'] + args, capture_output=True, text=True)
    print(json.dumps({'operation': args, 'exit': p.returncode, 'error': p.stderr}), flush=True)
    if args[0] in ['status', 'diff', 'log']:
        assert p.returncode == 0
try:
    pathlib.Path(os.environ['UNRELATED']).write_text('denied')
except PermissionError:
    print('unrelated-denied', flush=True)
else:
    raise AssertionError('unrelated write succeeded')
s = socket.socket()
s.settimeout(2)
result = s.connect_ex(('127.0.0.1', int(os.environ['PROBE_PORT'])))
print(json.dumps({'network_enabled': os.environ['PROBE_NETWORK'], 'connect_exit': result}), flush=True)
assert (result == 0) == (os.environ['PROBE_NETWORK'] == 'true')
try:
    print(json.dumps({'dns': socket.getaddrinfo('github.com', 443)}), flush=True)
except OSError as e:
    print(json.dumps({'dns_error': str(e)}), flush=True)
    assert os.environ['PROBE_NETWORK'] == 'false', 'DNS must work with network enabled'
"#;
    // Loopback comparison isolates network policy from external DNS outages.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect(
        "host sandbox probe needs loopback bind permission; run outside the enclosing seat sandbox",
    );
    for network in ["false", "true"] {
        let out = Command::new("codex")
            .current_dir(&fx.seat)
            .args(["sandbox", "-c", "sandbox_mode=\"workspace-write\"", "-c"])
            .arg(format!(
                "sandbox_workspace_write.writable_roots={}",
                serde_json::to_string(&roots).unwrap()
            ))
            .args([
                "-c",
                "sandbox_workspace_write.exclude_slash_tmp=true",
                "-c",
                "sandbox_workspace_write.exclude_tmpdir_env_var=true",
                "-c",
            ])
            .arg(format!("sandbox_workspace_write.network_access={network}"))
            .args(["/usr/bin/python3", "-c", script])
            .env("UNRELATED", unrelated.join("denied"))
            .env("PROBE_NETWORK", network)
            .env(
                "PROBE_PORT",
                listener.local_addr().unwrap().port().to_string(),
            )
            .output()
            .unwrap();
        println!(
            "network={network}: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "sandbox probe failed: {out:?}");
        assert!(!unrelated.join("denied").exists());
    }
    // Exercise the launcher's real preservation route, including the saved tree.
    checked(
        Command::new("git")
            .current_dir(&fx.seat)
            .args(["config", "user.name", "Fixture"]),
    );
    checked(Command::new("git").current_dir(&fx.seat).args([
        "config",
        "user.email",
        "fixture@example.invalid",
    ]));
    let saved = magi::git::rescue_commit(&fx.seat, "fixture: launcher save")
        .await
        .unwrap();
    let tree = checked(
        Command::new("git")
            .current_dir(&fx.seat)
            .args(["show", "HEAD:probe"]),
    );
    assert_eq!(String::from_utf8_lossy(&tree.stdout), "sandbox edit\n");
    assert!(saved.withheld.is_empty());
    println!(
        "launcher rescue committed={}, saved HEAD:probe verified",
        saved.committed
    );
}

struct Fixture {
    _tmp: tempfile::TempDir,
    primary: PathBuf,
    seat: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("primary");
        let seat = tmp.path().join("seat");
        let home = tmp.path().join("state");
        std::fs::create_dir(&primary).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "fixture",
            ],
            vec!["worktree", "add", "--detach", seat.to_str().unwrap()],
        ] {
            checked(Command::new("git").current_dir(&primary).args(args));
        }
        Self {
            _tmp: tmp,
            primary,
            seat,
            home,
        }
    }

    fn pwsh(&self, script: &str) -> Command {
        let mut cmd = Command::new("pwsh");
        cmd.current_dir(&self.seat)
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("MAGI_HOME", &self.home)
            .env_remove("MAGI_RUN")
            .env_remove("MAGI_NODE")
            .env_remove("MAGI_SEAT");
        cmd
    }
}

#[tokio::test]
async fn isolated_seat_scope_resolves_common_git_and_only_question_stores() {
    let fx = Fixture::new();
    let roots = worktree_writable_roots(&fx.seat, &fx.home).await.unwrap();
    assert_eq!(
        roots,
        [
            fx.primary.join(".git").canonicalize().unwrap(),
            fx.home.join("questions").canonicalize().unwrap(),
            fx.home.join("notifications").canonicalize().unwrap(),
        ]
    );
    assert!(fx.seat.join(".git").is_file());
    assert!(!roots.contains(&fx.home));
    assert!(!fx.home.join("queue").exists());
    #[cfg(unix)]
    {
        let alias = fx._tmp.path().join("alias");
        std::os::unix::fs::symlink(&fx.seat, &alias).unwrap();
        assert_eq!(
            worktree_writable_roots(&alias, &fx.home).await.unwrap(),
            roots
        );
    }
    // Non-checkout command seats still get the narrow question route.
    let plain_home = fx.home.join("plain");
    assert_eq!(
        worktree_writable_roots(fx._tmp.path(), &plain_home)
            .await
            .unwrap(),
        [
            plain_home.join("questions").canonicalize().unwrap(),
            plain_home.join("notifications").canonicalize().unwrap()
        ]
    );
    // A broken checkout is fatal before creating stores or starting work.
    let broken = fx._tmp.path().join("broken");
    std::fs::create_dir(&broken).unwrap();
    std::fs::write(broken.join(".git"), "gitdir: nonexistent\n").unwrap();
    assert!(
        worktree_writable_roots(&broken, &fx.home.join("bad"))
            .await
            .is_err()
    );
    assert!(!fx.home.join("bad").exists());
    // A primary checkout already covers its Git directory through cwd.
    // Canonicalization must also preserve this for a symlinked cwd.
    let expected = &roots[1..];
    assert_eq!(
        worktree_writable_roots(&fx.primary, &fx.home)
            .await
            .unwrap(),
        expected
    );
    #[cfg(unix)]
    {
        let alias = fx._tmp.path().join("primary-alias");
        std::os::unix::fs::symlink(&fx.primary, &alias).unwrap();
        assert_eq!(
            worktree_writable_roots(&alias, &fx.home).await.unwrap(),
            expected
        );
    }
}

/// Exercise OS enforcement without requiring an installed agent CLI or PowerShell.
/// Run explicitly on macOS outside an enclosing sandbox that forbids sandbox_apply.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires a host that can launch Seatbelt"]
async fn seatbelt_enforces_resolved_write_scope() {
    let fx = Fixture::new();
    let roots = worktree_writable_roots(&fx.seat, &fx.home).await.unwrap();
    let mut profile = String::from("(version 1)(allow default)(deny file-write*)");
    for root in std::iter::once(fx.seat.canonicalize().unwrap()).chain(roots) {
        // JSON quoting escapes the temporary path for Seatbelt's string syntax.
        profile.push_str(&format!(
            "(allow file-write* (subpath {}))",
            serde_json::to_string(root.to_str().unwrap()).unwrap()
        ));
    }
    let lock = fx.primary.join(".git/updater.lock");
    let question = fx.home.join("questions/probe.tmp");
    let work = fx.seat.join("work-started");
    let notification = fx.home.join("notifications/probe");
    let denied = fx.home.join("queue-probe");
    let run = |script: &str| {
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile, "/bin/sh", "-c", script, "scope-probe"])
            .args([&lock, &question, &work, &notification, &denied])
            .current_dir(&fx.seat)
            .output()
            .expect("launch Seatbelt")
    };
    // Pass paths as positional arguments, never interpolate them as shell code.
    let allowed = run(r#"echo lock > "$1" && echo question > "$2" &&
        mv "$2" "$2.saved" && echo work > "$3" && echo notice > "$4""#);
    assert!(allowed.status.success(), "{allowed:?}");
    assert!(lock.is_file());
    assert!(question.with_extension("tmp.saved").is_file());
    assert!(work.is_file());
    assert!(notification.is_file());
    let refused = run(r#"echo forbidden > "$5""#);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(!denied.exists());
}

common::e2e! {
#[cfg(unix)]
async fn invoked_editing_seats_receive_scope_on_first_turn_and_resume() {
    use magi::agent::{Invocation, SeatState, invoke};
    use magi::config::{AgentKind, AgentSpec};
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt as _;

    let _home_guard = common::home_lock().await;
    let fx = Fixture::new();
    magi::run::set_home(fx.home.clone());
    let bin = fx._tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let cli = bin.join("codex");
    std::fs::write(
        &cli,
        r#"#!/bin/sh
printf '%s\n' "$@" > "$SCOPE_ARGS"
printf '%s\n' "$MAGI_HOME" > "$SCOPE_HOME"
cat > /dev/null
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"fixture reply"}}'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    let args_path = fx._tmp.path().join("args");
    let home_path = fx._tmp.path().join("home");
    let spec = AgentSpec {
        id: "fixture".into(),
        kind: AgentKind::Codex,
        model: None,
        command: Vec::new(),
        extra_args: Vec::new(),
        prompt_delivery: None,
        env: BTreeMap::from([
            (
                "PATH".into(),
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            ),
            ("SCOPE_ARGS".into(), args_path.to_str().unwrap().into()),
            ("SCOPE_HOME".into(), home_path.to_str().unwrap().into()),
            ("MAGI_HOME".into(), "incorrect-store".into()),
        ]),
    };
    let fetched_base = magi::run::FetchedBase {
        remote: "origin".into(), branch: "main".into(),
        commit: "fixture-pinned-base".into(),
        fetched_at: "2026-10-18T00:00:00Z".parse().unwrap(),
    };
    let artifacts = fx._tmp.path().join("artifacts");
    for node in ["implement", "fix", "graft", "gate-fix", "land", "rebase", "chat"] {
        let mut seat = SeatState::new("fixture-seat", "fixture", 1);
        for resumed in [false, true] {
            if resumed {
                seat.captured_session = Some("fixture-session".into());
            }
            let out = invoke(
                &spec,
                &mut seat,
                &Invocation {
                    scope: magi::agent::InvocationScope::Repository,
                    coordination: Some(coordination()),
                    fetched_base: Some(&fetched_base),
                    cwd: &fx.seat,
                    prompt: "fixture",
                    timeout: Duration::from_secs(10),
                    allow_write: true,
                    unsandboxed: false,
                    sessions: true,
                    artifacts: &artifacts,
                    stem: node,
                    run: "fixture-run",
                    node,
                    cache_dir: None,
                    attachments: &[],
                    writable: &[],
                },
            )
            .await
            .unwrap();
            assert!(out.usable(), "{out:?}");
            let prompt = std::fs::read_to_string(artifacts.join(format!("{node}.prompt.md"))).unwrap();
            assert!(prompt.contains("fixture-pinned-base"), "{node}, resumed={resumed}");
            assert!(prompt.contains("Do not fetch again"));
            let access: serde_json::Value = serde_json::from_slice(
                &std::fs::read(artifacts.join(format!("{node}.access.json"))).unwrap(),
            ).unwrap();
            assert_eq!(access["fetched_base"]["commit"], "fixture-pinned-base");

            let args = std::fs::read_to_string(&args_path).unwrap();
            let roots = worktree_writable_roots(&fx.seat, &fx.home).await.unwrap();
            assert!(
                args.contains(&format!(
                    "sandbox_workspace_write.writable_roots={}",
                    serde_json::to_string(&roots).unwrap()
                )),
                "{node}, resumed={resumed}: {args}"
            );
            assert_eq!(args.lines().any(|s| s == "resume"), resumed);
            assert!(args.contains("workspace-write"));
            assert!(!args.contains("bypass"));
            assert_eq!(
                std::fs::read_to_string(&home_path).unwrap().trim(),
                fx.home.canonicalize().unwrap().to_str().unwrap()
            );
        }
    }
    // Other roles keep their explicit grants and home environment. Even a
    // writable deputy must not inherit shared repository metadata access.
    let explicit = [fx.home.join("questions").canonicalize().unwrap()];
    for (node, allow_write) in [("review", false), ("implement", false), ("deputy", true), ("chat", true)] {
        let mut seat = SeatState::new("fixture-seat", "fixture", 1);
        invoke(&spec, &mut seat, &Invocation {
            // A chat turn holding explicit store roots is a consultation turn.
            scope: if node == "chat" {
                magi::agent::InvocationScope::ConsultationControl
            } else {
                magi::agent::InvocationScope::Repository
            },
            coordination: Some(coordination()),
            fetched_base: None,
            cwd: &fx.seat, prompt: "fixture", timeout: Duration::from_secs(10),
            allow_write, unsandboxed: false, sessions: false,
            artifacts: &artifacts, stem: node, run: "fixture-run", node,
            cache_dir: None, attachments: &[], writable: &explicit,
        }).await.unwrap();
        let args = std::fs::read_to_string(&args_path).unwrap();
        assert!(!args.contains(fx.primary.join(".git").canonicalize().unwrap().to_str().unwrap()));
        assert_eq!(args.contains("writable_roots"), allow_write);
        assert_eq!(std::fs::read_to_string(&home_path).unwrap().trim(), "incorrect-store");
    }
}
}

// Same non-waiting FileStream/FileShare.None contract as the target repository.
// Never delete the file: all contenders must open the same inode.
const ACQUIRE: &str = r#"
$ErrorActionPreference = 'Stop'
$common = git rev-parse --path-format=absolute --git-common-dir
if ($LASTEXITCODE -ne 0) { exit 24 }
try {
  $lock = [IO.File]::Open((Join-Path $common 'updater.lock'),
    [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
} catch { Write-Output 'lock-refused'; exit 23 }
"#;

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn held_lock(fx: &Fixture) -> Running {
    let script = format!(
        "{ACQUIRE}\ntry {{ Write-Output 'held'; [Console]::ReadLine() | Out-Null }} finally {{ $lock.Dispose() }}"
    );
    let mut holder = Running(
        fx.pwsh(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(holder.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "held");
    holder
}

fn release_lock(holder: &mut Running) {
    writeln!(holder.0.stdin.take().unwrap()).unwrap();
    assert!(holder.0.wait().unwrap().success());
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires PowerShell to verify the real repository lock protocol"]
async fn readonly_invocation_holds_dotnet_compatible_lock_and_refuses_external_contention() {
    use magi::agent::{Invocation, SeatState, invoke};
    use magi::config::{AgentKind, AgentSpec};
    use std::collections::BTreeMap;

    let fx = Fixture::new();
    std::fs::write(
        fx.primary.join(".git/updater.contract"),
        "test-protocol-v1\n",
    )
    .unwrap();
    let script = r#"
$ErrorActionPreference = 'Stop'
try {
  $other = [IO.File]::Open($env:CHECK_LOCK, [IO.FileMode]::OpenOrCreate,
    [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
} catch { $denied = $true }
if (-not $denied) { $other.Dispose(); exit 25 }
[IO.File]::WriteAllText((Join-Path $PWD 'seat-started'), 'yes')
Write-Output 'real-lock-contends'
"#;
    let spec = AgentSpec {
        id: "fixture".into(),
        kind: AgentKind::Command,
        model: None,
        command: vec![
            "pwsh".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script.into(),
        ],
        extra_args: vec![],
        prompt_delivery: None,
        env: BTreeMap::from([(
            "CHECK_LOCK".into(),
            fx.primary
                .join(".git/updater.lock")
                .to_str()
                .unwrap()
                .into(),
        )]),
    };
    let artifacts = fx._tmp.path().join("reader-artifacts");
    let inv = Invocation {
        scope: magi::agent::InvocationScope::Repository,
        coordination: Some(coordination()),
        fetched_base: None,
        cwd: &fx.seat,
        prompt: "fixture read-only work",
        timeout: Duration::from_secs(10),
        allow_write: false,
        unsandboxed: false,
        sessions: false,
        artifacts: &artifacts,
        stem: "reader",
        run: "fixture-run",
        node: "review",
        cache_dir: None,
        attachments: &[],
        writable: &[],
    };
    let mut seat = SeatState::new("fixture-reader", "fixture", 1);
    let mut holder = held_lock(&fx);
    let out = {
        let attempt = invoke(&spec, &mut seat, &inv);
        tokio::pin!(attempt);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut attempt)
                .await
                .is_err()
        );
        assert!(!fx.seat.join("seat-started").exists());
        assert!(
            !artifacts.exists(),
            "resource wait precedes prompt creation"
        );
        release_lock(&mut holder);
        tokio::time::timeout(Duration::from_secs(10), &mut attempt)
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(seat.turns, 1, "contention was not an agent attempt");
    assert!(out.usable(), "{out:?}");
    assert!(out.text.contains("real-lock-contends"), "{out:?}");
    let prompt = std::fs::read_to_string(artifacts.join("reader.prompt.md")).unwrap();
    assert!(prompt.contains("launcher holds the exclusive repository lock"));
    assert!(prompt.contains("filesystem access remains read-only"));
    checked(&mut fx.pwsh(&format!("{ACQUIRE}\n$lock.Dispose()")));
}

#[cfg(unix)]
mod coordination {
    use super::*;
    use magi::agent::{Invocation, SeatState, invoke};
    use magi::config::{AgentKind, AgentSpec};
    use std::collections::BTreeMap;
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsRawFd as _;

    fn take_lock(path: &std::path::Path) -> Option<File> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        // SAFETY: this descriptor stays open for the entire returned File.
        (unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(f)
    }

    #[test]
    fn seat_child() {
        let Ok(mode) = std::env::var("COORDINATION_CHILD") else {
            return;
        };
        let marker = PathBuf::from(std::env::var_os("COORDINATION_MARKER").unwrap());
        match mode.as_str() {
            "reader" => {
                let mut tool = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "coordination::seat_child", "--nocapture"])
                    .env("COORDINATION_CHILD", "tool")
                    .spawn()
                    .unwrap();
                std::fs::write(
                    marker.with_extension("pids"),
                    format!("{} {}", std::process::id(), tool.id()),
                )
                .unwrap();
                tool.wait().unwrap();
            }
            "tool" => {
                // Terminal tools can live in a different process group from
                // their CLI. Cancellation must terminate these as well.
                // SAFETY: change only this fixture process's own group.
                assert_eq!(unsafe { libc::setpgid(0, 0) }, 0);
                std::fs::write(&marker, "ready").unwrap();
                std::thread::sleep(Duration::from_secs(30));
            }
            "editor" => {
                let path = PathBuf::from(std::env::var_os("COORDINATION_LOCK").unwrap());
                let _lock = take_lock(&path).expect("editor must not contend with a local reader");
                std::fs::write(marker, "edited").unwrap();
            }
            "holding-editor" => {
                let path = PathBuf::from(std::env::var_os("COORDINATION_LOCK").unwrap());
                let _lock = take_lock(&path).expect("editor takes the real lock");
                std::fs::write(marker, "ready").unwrap();
                std::thread::sleep(Duration::from_secs(30));
            }
            "holding-chat" => {
                let prompt =
                    std::fs::read_to_string(std::env::var_os("MAGI_PROMPT_FILE").unwrap()).unwrap();
                assert!(!prompt.contains("launcher holds the exclusive repository lock"));
                std::fs::write(&marker, "ready").unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while !marker.with_extension("release").exists() {
                    assert!(Instant::now() < deadline, "chat was not released");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            "quick" => {
                std::fs::write(marker, "ready").unwrap();
            }
            "locked-reader" => {
                let path = PathBuf::from(std::env::var_os("COORDINATION_LOCK").unwrap());
                assert!(
                    take_lock(&path).is_none(),
                    "launcher holds real lock for reader"
                );
                let prompt =
                    std::fs::read_to_string(std::env::var_os("MAGI_PROMPT_FILE").unwrap()).unwrap();
                assert!(prompt.contains("launcher holds the exclusive repository lock"));
                std::fs::write(marker, "ready").unwrap();
            }
            _ => panic!("unknown coordination helper"),
        }
    }

    fn spec(fx: &Fixture, mode: &str) -> AgentSpec {
        AgentSpec {
            id: "fixture".into(),
            kind: AgentKind::Command,
            model: None,
            command: vec![
                std::env::current_exe().unwrap().to_str().unwrap().into(),
                "--exact".into(),
                "coordination::seat_child".into(),
                "--nocapture".into(),
            ],
            extra_args: vec![],
            prompt_delivery: None,
            env: BTreeMap::from([
                ("COORDINATION_CHILD".into(), mode.into()),
                (
                    "COORDINATION_MARKER".into(),
                    fx.seat.join(mode).to_str().unwrap().into(),
                ),
                (
                    "COORDINATION_LOCK".into(),
                    fx.primary
                        .join(".git/updater.lock")
                        .to_str()
                        .unwrap()
                        .into(),
                ),
            ]),
        }
    }

    fn invocation<'a>(
        fx: &'a Fixture,
        art: &'a std::path::Path,
        allow_write: bool,
    ) -> Invocation<'a> {
        Invocation {
            scope: magi::agent::InvocationScope::Repository,
            coordination: Some(coordination()),
            fetched_base: None,
            cwd: &fx.seat,
            prompt: "fixture",
            timeout: Duration::from_secs(20),
            allow_write,
            unsandboxed: false,
            sessions: false,
            artifacts: art,
            stem: "fixture",
            run: "fixture",
            node: "test",
            cache_dir: None,
            attachments: &[],
            writable: &[],
        }
    }

    fn registered() -> Fixture {
        let fx = Fixture::new();
        std::fs::write(
            fx.primary.join(".git/updater.contract"),
            "test-protocol-v1\n",
        )
        .unwrap();
        fx
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal zero only probes the recorded fixture process.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    // Each child reports startup and waits for an explicit release. Observing
    // both markers before release proves overlapping invocations, not merely
    // two quick successful turns. Separate artifact directories avoid races.
    async fn concurrent_chats(fx: &Fixture, policy: Option<&magi::config::Coordination>) {
        for allow_write in [false, true] {
            let first_marker = fx.seat.join(format!("first-chat-{allow_write}"));
            let second_marker = fx.seat.join(format!("second-chat-{allow_write}"));
            let mut first_spec = spec(fx, "holding-chat");
            first_spec.env.insert(
                "COORDINATION_MARKER".into(),
                first_marker.to_str().unwrap().into(),
            );
            let mut second_spec = first_spec.clone();
            second_spec.env.insert(
                "COORDINATION_MARKER".into(),
                second_marker.to_str().unwrap().into(),
            );
            let first_art = fx._tmp.path().join(format!("first-art-{allow_write}"));
            let second_art = fx._tmp.path().join(format!("second-art-{allow_write}"));
            let first_inv = Invocation {
                node: magi::queue::CHAT_NODE,
                coordination: policy,
                ..invocation(fx, &first_art, allow_write)
            };
            let second_inv = Invocation {
                artifacts: &second_art,
                ..first_inv.clone()
            };
            let mut first_seat = SeatState::new("first-chat", "fixture", 1);
            let mut second_seat = SeatState::new("second-chat", "fixture", 1);
            let mut first = Box::pin(invoke(&first_spec, &mut first_seat, &first_inv));
            let mut second = Box::pin(invoke(&second_spec, &mut second_seat, &second_inv));
            tokio::time::timeout(Duration::from_secs(5), async {
                while !first_marker.exists() || !second_marker.exists() {
                    tokio::select! {
                        out = &mut first => panic!("first chat exited before release: {out:?}"),
                        out = &mut second => panic!("second chat exited before release: {out:?}"),
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                    }
                }
            })
            .await
            .expect("both chats must spawn without waiting for repository coordination");
            for marker in [&first_marker, &second_marker] {
                std::fs::write(marker.with_extension("release"), "go").unwrap();
            }
            let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(first, second)
            })
            .await
            .unwrap();
            assert!(first.unwrap().usable());
            assert!(second.unwrap().usable());
            assert_eq!(first_seat.turns, 1);
            assert_eq!(second_seat.turns, 1);
        }
    }

    #[tokio::test]
    async fn chats_spawn_concurrently_despite_external_shared_lock() {
        let fx = Fixture::new();
        let mut policy = coordination().clone();
        policy.contract_token = "shared-lock-v1".into();
        std::fs::write(
            fx.primary.join(".git").join(&policy.contract_file),
            &policy.contract_token,
        )
        .unwrap();
        let external = take_lock(&fx.primary.join(".git").join(&policy.lock_file)).unwrap();
        let run_spec = spec(&fx, "locked-reader");
        let run_art = fx._tmp.path().join("run-art");
        let run_inv = Invocation {
            node: "implement",
            coordination: Some(&policy),
            ..invocation(&fx, &run_art, false)
        };
        let mut run_seat = SeatState::new("implement", "fixture", 1);
        let mut run = Box::pin(invoke(&run_spec, &mut run_seat, &run_inv));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut run)
                .await
                .is_err()
        );
        assert!(!run_art.exists(), "implement must still wait before spawn");
        concurrent_chats(&fx, Some(&policy)).await;
        assert!(!run_art.exists());
        drop(external);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), run)
                .await
                .unwrap()
                .unwrap()
                .usable()
        );
        assert_eq!(run_seat.turns, 1);
    }

    #[tokio::test]
    async fn chats_spawn_concurrently_without_coordination() {
        concurrent_chats(&Fixture::new(), None).await;
    }

    #[tokio::test]
    async fn machine_config_preserves_existing_external_lock_protocol() {
        let fx = Fixture::new();
        // Literal migration fixture: these are existing external files, never defaults.
        let machine = fx._tmp.path().join("machine.toml");
        std::fs::write(
            &machine,
            r#"
[coordination]
contract_file = "kanade-sync.contract"
contract_token = "shared-lock-v1"
lock_file = "kanade-sync.lock"
"#,
        )
        .unwrap();
        let cfg = magi::config::Config::load(&machine).unwrap();
        let policy = cfg.coordination.as_ref().unwrap();
        let common = fx.primary.join(".git");
        std::fs::write(
            common.join(&policy.contract_file),
            format!("{}\n", policy.contract_token),
        )
        .unwrap();
        let external = take_lock(&common.join(&policy.lock_file)).unwrap();
        let mut spec = spec(&fx, "locked-reader");
        spec.env.insert(
            "COORDINATION_LOCK".into(),
            common.join(&policy.lock_file).to_str().unwrap().into(),
        );
        let art = fx._tmp.path().join("art");
        let inv = Invocation {
            scope: magi::agent::InvocationScope::Repository,
            coordination: cfg.coordination.as_ref(),
            fetched_base: None,
            ..invocation(&fx, &art, false)
        };
        let mut seat = SeatState::new("reader", "fixture", 1);
        let future = invoke(&spec, &mut seat, &inv);
        tokio::pin!(future);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut future)
                .await
                .is_err()
        );
        assert!(!art.exists());
        drop(external);
        let out = tokio::time::timeout(Duration::from_secs(10), &mut future)
            .await
            .unwrap()
            .unwrap();
        assert!(out.usable(), "{out:?}");
        assert!(take_lock(&common.join(&policy.lock_file)).is_some());
    }

    #[tokio::test]
    async fn control_plane_progresses_while_editor_holds_reservation_and_real_lock() {
        let fx = registered();
        let editor_spec = spec(&fx, "holding-editor");
        let art = fx._tmp.path().join("art");
        let write_inv = Invocation {
            node: "implement",
            ..invocation(&fx, &art, true)
        };
        let mut editor_seat = SeatState::new("editor", "fixture", 1);
        let mut editor = Box::pin(invoke(&editor_spec, &mut editor_seat, &write_inv));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fx.seat.join("holding-editor").exists() {
            tokio::select! {
                out = &mut editor => panic!("editor exited early: {out:?}"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
            assert!(Instant::now() < deadline);
        }
        let lock = fx.primary.join(".git/updater.lock");
        assert!(take_lock(&lock).is_none());
        let consult_roots = [fx.home.join("questions")];
        // Both ordinary chats can start while the implement seat owns the
        // local editor reservation and real OS lock.
        concurrent_chats(&fx, Some(coordination())).await;
        // Consulted chats retain their control-only prompt and grants.
        for node in ["conduct", "dupes", "chat", "deputy"] {
            for allow_write in [false, true] {
                if node == "chat" && !allow_write {
                    continue;
                }
                let marker = fx.home.join(format!("{node}-{allow_write}"));
                std::fs::create_dir_all(&fx.home).unwrap();
                let mut control_spec = spec(&fx, "quick");
                control_spec.env.insert(
                    "COORDINATION_MARKER".into(),
                    marker.to_str().unwrap().into(),
                );
                let control_inv = Invocation {
                    node,
                    scope: magi::agent::InvocationScope::ConsultationControl,
                    writable: if node == "chat" { &consult_roots } else { &[] },
                    ..invocation(&fx, &art, allow_write)
                };
                let mut seat = SeatState::new(node, "fixture", 1);
                let out = tokio::time::timeout(
                    Duration::from_secs(5),
                    invoke(&control_spec, &mut seat, &control_inv),
                )
                .await
                .expect("control plane must not wait for editor")
                .unwrap();
                assert!(out.usable(), "{node}: {out:?}");
                assert!(marker.exists());
                if node == "chat" {
                    let prompt = std::fs::read_to_string(art.join("fixture.prompt.md")).unwrap();
                    assert!(prompt.contains("Current turn scope: consultation control"));
                    assert!(prompt.contains("do not inspect repository files/history"));
                }
                assert!(take_lock(&lock).is_none(), "editor still owns real lock");
            }
        }
        let investigation_spec = spec(&fx, "quick");
        let investigation_inv = Invocation {
            scope: magi::agent::InvocationScope::ConsultationInvestigation,
            node: "chat",
            ..invocation(&fx, &art, false)
        };
        let mut investigation_seat = SeatState::new("chat", "fixture", 1);
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            invoke(
                &investigation_spec,
                &mut investigation_seat,
                &investigation_inv,
            ),
        )
        .await
        .expect("investigation must not wait behind the asking editor")
        .unwrap_err();
        assert!(error.to_string().contains("reserved"));
        assert_eq!(investigation_seat.turns, 0);
        assert!(
            take_lock(&lock).is_none(),
            "original editor still owns its lock"
        );
        // Repository readers must still respect the editor's lock, even with
        // a short decision budget; control-plane exclusions do not widen them.
        for (node, allow_write) in [
            ("judge", false),
            ("advise", false),
            ("review", false),
            ("implement", false),
            ("implement", true),
        ] {
            let reader_spec = spec(&fx, "quick");
            let reader_inv = Invocation {
                node,
                ..invocation(&fx, &art, allow_write)
            };
            let mut seat = SeatState::new(node, "fixture", 1);
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(100),
                    invoke(&reader_spec, &mut seat, &reader_inv),
                )
                .await
                .is_err()
            );
            assert_eq!(seat.turns, 0);
            assert!(!fx.seat.join("quick").exists());
        }
        drop(editor);
        while take_lock(&lock).is_none() {
            assert!(Instant::now() < deadline, "editor cleanup must finish");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn cancellation_keeps_lock_until_cli_and_tool_have_terminated() {
        let fx = registered();
        let spec = spec(&fx, "reader");
        let art = fx._tmp.path().join("art");
        let inv = Invocation {
            scope: magi::agent::InvocationScope::ConsultationInvestigation,
            ..invocation(&fx, &art, false)
        };
        let mut seat = SeatState::new("reader", "fixture", 1);
        let deadline = Instant::now() + Duration::from_secs(10);
        {
            let future = invoke(&spec, &mut seat, &inv);
            tokio::pin!(future);
            while !fx.seat.join("reader").exists() {
                tokio::select! {
                    out = &mut future => panic!("reader exited early: {out:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
                assert!(Instant::now() < deadline);
            }
        } // same future-drop path as waiter halt and chat lease loss
        let pids: Vec<i32> = std::fs::read_to_string(fx.seat.join("reader.pids"))
            .unwrap()
            .split_whitespace()
            .map(|p| p.parse().unwrap())
            .collect();
        loop {
            if let Some(lock) = take_lock(&fx.primary.join(".git/updater.lock")) {
                assert!(
                    pids.iter().all(|pid| !alive(*pid)),
                    "lock released while a seat process lives"
                );
                drop(lock);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cancelled seat did not finish cleanup"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(seat.turns, 0);
    }

    #[tokio::test]
    async fn editing_seat_waits_for_reader_cleanup_then_acquires_its_real_lock() {
        let fx = registered();
        let reader_spec = spec(&fx, "reader");
        let editor_spec = spec(&fx, "editor");
        let art = fx._tmp.path().join("art");
        let read_inv = invocation(&fx, &art, false);
        let write_inv = Invocation {
            node: "implement",
            ..invocation(&fx, &art, true)
        };
        let mut reader_seat = SeatState::new("reader", "fixture", 1);
        let mut editor_seat = SeatState::new("editor", "fixture", 1);
        let reader = invoke(&reader_spec, &mut reader_seat, &read_inv);
        let editor = invoke(&editor_spec, &mut editor_seat, &write_inv);
        let mut reader = Box::pin(reader);
        let mut editor = Box::pin(editor);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fx.seat.join("reader").exists() {
            tokio::select! {
                out = &mut reader => panic!("reader exited early: {out:?}"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
            assert!(Instant::now() < deadline);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut editor)
                .await
                .is_err()
        );
        assert!(
            !fx.seat.join("editor").exists(),
            "editor must not start while reader owns the lock"
        );
        let later_spec = spec(&fx, "quick");
        let mut later_seat = SeatState::new("later-reader", "fixture", 1);
        let mut later_reader = Box::pin(invoke(&later_spec, &mut later_seat, &read_inv));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut later_reader)
                .await
                .is_err()
        );
        assert!(
            !fx.seat.join("quick").exists(),
            "new readers must not overtake a queued editor"
        );
        // Question/control-plane delivery must remain available while a repo
        // seat is waiting for the owner's answer. It performs no repo work.
        let questions = fx.home.join("questions");
        std::fs::create_dir_all(&questions).unwrap();
        let mut deputy_spec = spec(&fx, "quick");
        deputy_spec.env.insert(
            "COORDINATION_MARKER".into(),
            questions.join("control-plane").to_str().unwrap().into(),
        );
        let roots = [questions.clone()];
        let deputy_inv = Invocation {
            allow_write: true,
            node: "deputy",
            writable: &roots,
            ..invocation(&fx, &art, true)
        };
        let mut deputy_seat = SeatState::new("deputy", "fixture", 1);
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            invoke(&deputy_spec, &mut deputy_seat, &deputy_inv),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(out.usable(), "{out:?}");
        assert!(questions.join("control-plane").exists());
        drop(reader);
        let out = tokio::time::timeout(Duration::from_secs(10), &mut editor)
            .await
            .unwrap()
            .unwrap();
        assert!(out.usable(), "{out:?}");
        assert!(fx.seat.join("editor").exists());
        assert!(
            tokio::time::timeout(Duration::from_secs(10), &mut later_reader)
                .await
                .unwrap()
                .unwrap()
                .usable()
        );
        drop(editor);
        assert_eq!(editor_seat.turns, 1);
    }

    #[tokio::test]
    async fn external_contention_waits_without_spawning_or_charging_an_attempt() {
        let fx = registered();
        let external = take_lock(&fx.primary.join(".git/updater.lock")).unwrap();
        let spec = spec(&fx, "locked-reader");
        let art = fx._tmp.path().join("art");
        let inv = Invocation {
            node: "review",
            ..invocation(&fx, &art, false)
        };
        let mut seat = SeatState::new("reader", "fixture", 1);
        {
            let future = invoke(&spec, &mut seat, &inv);
            tokio::pin!(future);
            assert!(
                tokio::time::timeout(Duration::from_millis(150), &mut future)
                    .await
                    .is_err()
            );
            assert!(!art.exists());
            assert!(!fx.seat.join("locked-reader").exists());
            drop(external);
            let out = tokio::time::timeout(Duration::from_secs(10), &mut future)
                .await
                .unwrap()
                .unwrap();
            assert!(out.usable(), "{out:?}");
        }
        assert_eq!(seat.turns, 1, "only the eventual invocation counts");
    }
}

#[test]
#[ignore = "requires PowerShell; run explicitly on the deployment host"]
fn shared_lock_contention_prevents_work_and_release_allows_reacquisition() {
    let fx = Fixture::new();
    let mut holder = held_lock(&fx);
    let work = format!(
        "{ACQUIRE}\ntry {{ [IO.File]::WriteAllText((Join-Path $PWD 'work-started'), 'yes') }} finally {{ $lock.Dispose() }}"
    );
    let refused = fx.pwsh(&work).output().unwrap();
    assert_eq!(refused.status.code(), Some(23), "{refused:?}");
    assert!(!fx.seat.join("work-started").exists());
    release_lock(&mut holder);
    checked(&mut fx.pwsh(&work));
    assert!(fx.seat.join("work-started").is_file());
    assert!(fx.primary.join(".git/updater.lock").is_file());
}

#[tokio::test]
#[ignore = "requires PowerShell and a host that can launch the real CLI sandbox"]
async fn sandbox_seat_can_lock_ask_and_receive_answer_but_cannot_write_unrelated_state() {
    question_route(true).await;
}

#[tokio::test]
#[ignore = "requires PowerShell; validates the supported CLI route without a sandbox"]
async fn host_seat_can_lock_ask_and_receive_answer() {
    question_route(false).await;
}

async fn question_route(sandbox: bool) {
    let fx = Fixture::new();
    let roots = worktree_writable_roots(&fx.seat, &fx.home).await.unwrap();
    let unrelated = fx.home.join("queue");
    std::fs::create_dir(&unrelated).unwrap();
    let denial = if sandbox {
        r#"try { [IO.File]::WriteAllText($env:UNRELATED, 'bad'); exit 25 }
  catch { Write-Output 'unrelated-denied' }"#
    } else {
        ""
    };
    let script = format!(
        r#"{ACQUIRE}
try {{
  {denial}
  & $env:MAGI_TEST_BINARY ask --repo . --summary 'fixture question' --detail fixture --timeout 30
  if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}
}} finally {{ $lock.Dispose() }}
"#
    );
    let mut cmd = if sandbox {
        Command::new("codex")
    } else {
        Command::new("pwsh")
    };
    if sandbox {
        cmd.args(["sandbox", "-c", "sandbox_mode=\"workspace-write\"", "-c"])
            .arg(format!(
                "sandbox_workspace_write.writable_roots={}",
                serde_json::to_string(&roots).unwrap()
            ))
            .args([
                "-c",
                "sandbox_workspace_write.exclude_slash_tmp=true",
                "-c",
                "sandbox_workspace_write.exclude_tmpdir_env_var=true",
            ])
            .arg("pwsh");
    }
    let cli = common::magi_cli(&fx.home);
    for (key, value) in cli.get_envs() {
        if let Some(value) = value {
            cmd.env(key, value);
        } else {
            cmd.env_remove(key);
        }
    }
    cmd.current_dir(&fx.seat)
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .env("MAGI_TEST_BINARY", cli.get_program())
        .env("UNRELATED", unrelated.join("denied"))
        .env_remove("MAGI_RUN")
        .env_remove("MAGI_NODE")
        .env_remove("MAGI_SEAT")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let store = Questions::at(fx.home.join("questions"));
    if sandbox {
        let mut holder = held_lock(&fx);
        let refused = cmd.output().unwrap();
        assert_eq!(refused.status.code(), Some(23), "{refused:?}");
        assert!(
            store.list().is_empty(),
            "no question before lock acquisition"
        );
        release_lock(&mut holder);
    }
    let mut child = Running(cmd.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(25);
    let question = loop {
        if let Some(q) = store.list().into_iter().next() {
            break q;
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            let out = child.0.stderr.take().unwrap();
            panic!(
                "sandbox exited before filing a question: {status}; {}",
                std::io::read_to_string(out).unwrap()
            );
        }
        assert!(Instant::now() < deadline, "question was not created");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    checked(
        common::magi_cli(&fx.home)
            .current_dir(&fx.seat)
            .env_remove("MAGI_RUN")
            .env_remove("MAGI_NODE")
            .env_remove("MAGI_SEAT")
            .args(["answer", &question.id, "--reply", "fixture-answer"]),
    );
    while child.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "ask did not receive its answer");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(child.0.wait().unwrap().success());
    let stdout = std::io::read_to_string(child.0.stdout.take().unwrap()).unwrap();
    assert!(stdout.contains("fixture-answer"), "{stdout}");
    if sandbox {
        assert!(stdout.contains("unrelated-denied"), "{stdout}");
    }
    assert!(!unrelated.join("denied").exists());
    assert!(fx.primary.join(".git/updater.lock").is_file());
}
