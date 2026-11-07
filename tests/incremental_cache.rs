//! Exercise the real Cargo -> existing wrapper -> compiler chain in isolation.
#![cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[tokio::test]
async fn setup_timeout_joins_compiler_groups_with_and_without_incremental() {
    use magi::config::{Config, SetupStep};
    use std::time::Duration;
    for incremental in ["0", "1"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec![
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        ] {
            assert!(
                Command::new("git")
                    .current_dir(&repo)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let worktree = root.join("worktree");
        assert!(
            Command::new("git")
                .current_dir(&repo)
                .args(["worktree", "add", "--detach"])
                .arg(&worktree)
                .arg("HEAD")
                .output()
                .unwrap()
                .status
                .success()
        );
        fs::create_dir(worktree.join("src")).unwrap();
        fs::write(worktree.join("Cargo.toml"),
            "[package]\nname='setup-timeout-fixture'\nversion='0.1.0'\nedition='2021'\n[workspace]\n").unwrap();
        fs::write(
            worktree.join("src/lib.rs"),
            "pub fn answer() -> u32 { 42 }\n",
        )
        .unwrap();
        let quote =
            |path: &std::path::Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
        fs::write(
            worktree.join("manager-wrapper.sh"),
            format!(
                "#!/bin/sh\nexec {} __magi_incremental \"$@\"\n",
                quote(std::path::Path::new(env!("CARGO_BIN_EXE_magi")))
            ),
        )
        .unwrap();
        // Cargo's real version probes run normally. Its compilation enters
        // this long-lived compiler wrapper in the manager's separate group.
        fs::write(
            worktree.join("compiler-delay.sh"),
            r#"#!/bin/sh
for argument do
    if [ "$argument" = --crate-name ]; then
        echo $$ > "$PWD/compiler.pid"
        while :; do echo writing >> "$PWD/pulse"; sleep 0.02; done
    fi
done
exec "$@"
"#,
        )
        .unwrap();
        fs::write(
            worktree.join("setup-fixture.sh"),
            format!(
                r#"#!/bin/sh
export RUSTUP_TOOLCHAIN=stable
export CARGO_HOME="$PWD/cargo-home"
export CARGO_INCREMENTAL={incremental}
export RUSTC_WRAPPER="$PWD/manager-wrapper.sh"
export MAGI_INCREMENTAL_WORKTREE="$PWD"
export MAGI_PREVIOUS_RUSTC_WRAPPER="$PWD/compiler-delay.sh"
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS RUSTC_WORKSPACE_WRAPPER
exec cargo build --offline --jobs 1 --target-dir "$PWD/target"
"#
            ),
        )
        .unwrap();
        for name in ["manager-wrapper.sh", "compiler-delay.sh"] {
            fs::set_permissions(worktree.join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        struct Cleanup {
            unrelated: std::process::Child,
            compiler_pid: std::path::PathBuf,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Ok(pid) = fs::read_to_string(&self.compiler_pid)
                    .unwrap_or_default()
                    .trim()
                    .parse::<i32>()
                    && pid > 0
                    && magi::proc::pid_alive(pid as u32)
                {
                    // SAFETY: the private fixture records only its own compiler.
                    unsafe {
                        if libc::getpgid(pid) == pid {
                            libc::kill(-pid, libc::SIGKILL);
                        }
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
                self.unrelated.kill().ok();
                self.unrelated.wait().ok();
            }
        }
        let mut cleanup = Cleanup {
            unrelated: Command::new("sleep").arg("30").spawn().unwrap(),
            compiler_pid: worktree.join("compiler.pid"),
        };
        let mut cfg = Config::default();
        cfg.verify.shell = Some(vec!["sh".into(), "-c".into()]);
        cfg.worktree.setup = vec![SetupStep {
            run: Some("sh ./setup-fixture.sh".into()),
            timeout_secs: Some(5),
            ..Default::default()
        }];
        let result = magi::worktree_setup::prepare(&cfg, &repo, &worktree).await;
        let pid = fs::read_to_string(&cleanup.compiler_pid)
            .unwrap_or_default()
            .trim()
            .parse::<u32>()
            .ok();
        let alive = pid.is_some_and(magi::proc::pid_alive);
        let before = fs::read(worktree.join("pulse")).unwrap_or_default();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let after = fs::read(worktree.join("pulse")).unwrap_or_default();
        let unrelated_alive = cleanup.unrelated.try_wait().unwrap().is_none();
        drop(cleanup);
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("timed out"), "{error}");
        assert!(
            pid.is_some() && !before.is_empty(),
            "Cargo never reached the compiler wrapper"
        );
        assert!(
            !alive,
            "setup returned with a live compiler (incremental={incremental})"
        );
        assert_eq!(
            before, after,
            "compiler writes continued after setup timeout"
        );
        assert!(
            unrelated_alive,
            "setup cleanup stopped an unrelated process"
        );
    }
}

#[test]
fn explicit_incremental_uses_proven_generations_and_keeps_existing_wrappers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    assert!(
        Command::new("git")
            .arg("init")
            .arg(&root)
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='wrapper-fixture'\nversion='0.1.0'\nedition='2021'\n[workspace]\nmembers=['member0','member1','member2']\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
    for member in 0..3 {
        let path = root.join(format!("member{member}"));
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(
            path.join("Cargo.toml"),
            format!(
                "[package]\nname='parallel-member-{member}'\nversion='0.1.0'\nedition='2021'\n"
            ),
        )
        .unwrap();
        fs::write(
            path.join("src/lib.rs"),
            format!("pub fn value() -> u32 {{ {member} }}\n"),
        )
        .unwrap();
    }
    let script = root.join("wrapper.sh");
    fs::write(
        &script,
        "#!/bin/sh\nfor argument do\n case \"$argument\" in incremental=*|-Cincremental=*) printf '%s\\n' started >> \"$FIXTURE_ENTRY_LOG\";; esac\ndone\nexec \"$FIXTURE_MAGI\" __magi_incremental \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let previous = root.join("previous.sh");
    fs::write(
        &previous,
        "#!/bin/sh\nprintf '%s\\n' invoked >> \"$FIXTURE_WRAPPER_LOG\"\nexec \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&previous, fs::Permissions::from_mode(0o700)).unwrap();
    let workspace = root.join("workspace.sh");
    fs::write(
        &workspace,
        "#!/bin/sh\nprintf '%s\\n' workspace >> \"$FIXTURE_WRAPPER_LOG\"\nexec \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700)).unwrap();
    let legacy = root.join("target/magi-tools");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("operator-input"), "retain").unwrap();
    // Force actual compiler wrappers to overlap at admission, independently of
    // host scheduling speed. Both must survive this temporary namespace lock.
    let store = root.join(".git/magi-incremental");
    fs::create_dir(&store).unwrap();
    let admission = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(store.join("admission.lock"))
        .unwrap();
    use std::os::fd::AsRawFd;
    // SAFETY: private fixture file; closing it releases this test's lock.
    assert_eq!(
        unsafe { libc::flock(admission.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let entries = root.join("entries.log");
    let mut cargo = Command::new("cargo");
    cargo.env_clear();
    for key in ["PATH", "HOME", "RUSTUP_HOME"] {
        if let Some(value) = std::env::var_os(key) {
            cargo.env(key, value);
        }
    }
    let mut build = cargo
        .current_dir(&root)
        .env("RUSTUP_TOOLCHAIN", "stable")
        .env("CARGO_HOME", root.join("cargo-home"))
        .env("CARGO_INCREMENTAL", "1")
        .env("RUSTC_WRAPPER", script)
        .env("RUSTC_WORKSPACE_WRAPPER", workspace)
        .env("MAGI_PREVIOUS_RUSTC_WRAPPER", previous)
        .env("MAGI_INCREMENTAL_WORKTREE", &root)
        .env("FIXTURE_MAGI", env!("CARGO_BIN_EXE_magi"))
        .env("FIXTURE_WRAPPER_LOG", root.join("wrapper.log"))
        .env("FIXTURE_ENTRY_LOG", &entries)
        .args([
            "build",
            "--offline",
            "--workspace",
            "--jobs",
            "3",
            "--target-dir",
        ])
        .arg(root.join("target"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    let overlapped = loop {
        if fs::read_to_string(&entries)
            .unwrap_or_default()
            .lines()
            .count()
            >= 2
        {
            break true;
        }
        if build.try_wait().unwrap().is_some()
            || started.elapsed() > std::time::Duration::from_secs(30)
        {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    // The entry marker precedes exec; leave time for both warmed wrapper
    // processes to reach their admission attempts while the lock stays held.
    std::thread::sleep(std::time::Duration::from_millis(250));
    drop(admission);
    let output = build.wait_with_output().unwrap();
    assert!(overlapped, "parallel compiler entries did not overlap");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // More than four successful compiler invocations must remain admitted
    // while the completed caches are small, without waiting for run death.
    for value in 100..105 {
        fs::write(
            root.join("src/lib.rs"),
            format!("pub fn answer() -> u32 {{ {value} }}\n"),
        )
        .unwrap();
        for member in 0..3 {
            fs::write(
                root.join(format!("member{member}/src/lib.rs")),
                format!("pub fn value() -> u32 {{ {value} }}\n"),
            )
            .unwrap();
        }
        let output = cargo.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let log = fs::read_to_string(root.join("wrapper.log")).unwrap();
    assert!(log.contains("invoked"));
    assert!(log.contains("workspace"));
    let records: Vec<_> = fs::read_dir(root.join(".git/magi-incremental"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    assert_eq!(records.len(), 24);
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(&records[0]).unwrap()).unwrap();
    assert!(!record["files"].as_object().unwrap().is_empty());
    assert!(
        record["generation"]
            .as_str()
            .unwrap()
            .contains("/incremental/magi-")
    );
    assert_eq!(
        fs::read_to_string(legacy.join("operator-input")).unwrap(),
        "retain"
    );
}

#[test]
fn budget_failure_waits_for_a_non_exec_wrappers_compiler_descendant() {
    use std::time::{Duration, Instant};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    assert!(
        Command::new("git")
            .arg("init")
            .arg(&root)
            .output()
            .unwrap()
            .status
            .success()
    );
    let parent = root.join("target/debug/incremental");
    fs::create_dir_all(&parent).unwrap();
    let previous = root.join("waiting-wrapper.sh");
    fs::write(
        &previous,
        "#!/bin/sh\necho $$ > \"$FIXTURE_WRAPPER_PID\"\n\"$@\" &\nwait $!\n",
    )
    .unwrap();
    let compiler = root.join("compiler.sh");
    fs::write(
        &compiler,
        r#"#!/bin/sh
for argument do
    case "$argument" in incremental=*) cache=${argument#incremental=};; esac
done
echo $$ > "$FIXTURE_COMPILER_PID"
echo writing > "$FIXTURE_PULSE"
dd if=/dev/zero of="$cache/oversized" bs=1048576 count=1 seek=128 2>/dev/null
while :; do echo writing >> "$FIXTURE_PULSE"; sleep 0.02; done
"#,
    )
    .unwrap();
    for path in [&previous, &compiler] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let wrapper_pid = root.join("wrapper.pid");
    let compiler_pid = root.join("compiler.pid");
    let pulse = root.join("pulse");
    struct OwnedProcesses(Vec<i32>);
    impl Drop for OwnedProcesses {
        fn drop(&mut self) {
            for pid in &self.0 {
                // SAFETY: PIDs written by this test's private helper processes.
                unsafe {
                    libc::kill(*pid, libc::SIGKILL);
                }
            }
        }
    }
    let mut cleanup = OwnedProcesses(Vec::new());
    let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_magi"))
        .arg("__magi_incremental")
        .arg(&compiler)
        .arg("-C")
        .arg(format!("incremental={}", parent.display()))
        .env("MAGI_INCREMENTAL_WORKTREE", &root)
        .env("MAGI_PREVIOUS_RUSTC_WRAPPER", previous)
        .env("FIXTURE_WRAPPER_PID", &wrapper_pid)
        .env("FIXTURE_COMPILER_PID", &compiler_pid)
        .env("FIXTURE_PULSE", &pulse)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let status = loop {
        for path in [&wrapper_pid, &compiler_pid] {
            if let Ok(text) = fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse::<i32>()
                && !cleanup.0.contains(&pid)
            {
                cleanup.0.push(pid);
            }
        }
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if started.elapsed() > Duration::from_secs(15) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let unrelated_alive = unrelated.try_wait().unwrap().is_none();
    unrelated.kill().ok();
    unrelated.wait().unwrap();
    let descendants_gone = cleanup.0.len() == 2
        && cleanup.0.iter().all(|pid| {
            // SAFETY: signal zero observes this test's recorded helper PID only.
            (unsafe { libc::kill(*pid, 0) }) == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        });
    if status.is_none() {
        child.kill().ok();
        child.wait().unwrap();
    }
    assert!(
        status.is_some_and(|s| !s.success()),
        "budget monitor did not finish"
    );
    assert!(
        descendants_gone,
        "wrapper returned while its compiler was still alive"
    );
    cleanup.0.clear();
    assert!(unrelated_alive);
    let before = fs::read(&pulse).unwrap();
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(fs::read(&pulse).unwrap(), before);
    assert!(
        fs::read_dir(root.join(".git/magi-incremental"))
            .unwrap()
            .all(|e| e.unwrap().path().extension().is_none_or(|e| e != "json"))
    );
}
