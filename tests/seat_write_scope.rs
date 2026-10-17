//! Real shared-directory and control-plane fixtures. No operator state is used.
mod common;

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
    // Bad repository resolution is fatal before creating stores or starting work.
    assert!(
        worktree_writable_roots(fx._tmp.path(), &fx.home.join("bad"))
            .await
            .is_err()
    );
    assert!(!fx.home.join("bad").exists());
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
    let artifacts = fx._tmp.path().join("artifacts");
    for node in ["implement", "fix", "graft", "gate-fix", "land", "rebase"] {
        let mut seat = SeatState::new("fixture-seat", "fixture", 1);
        for resumed in [false, true] {
            if resumed {
                seat.captured_session = Some("fixture-session".into());
            }
            let out = invoke(
                &spec,
                &mut seat,
                &Invocation {
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
  $lock = [IO.File]::Open((Join-Path $common 'kanade-sync.lock'),
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
    assert!(fx.primary.join(".git/kanade-sync.lock").is_file());
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
    cmd.current_dir(&fx.seat)
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .env("MAGI_HOME", &fx.home)
        .env("MAGI_TEST_BINARY", env!("CARGO_BIN_EXE_magi"))
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
        Command::new(env!("CARGO_BIN_EXE_magi"))
            .current_dir(&fx.seat)
            .env("MAGI_HOME", &fx.home)
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
    assert!(fx.primary.join(".git/kanade-sync.lock").is_file());
}
