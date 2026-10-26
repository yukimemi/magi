//! A blocked background release check must not hold the CLI process open.

#![cfg(unix)]

mod common;

use std::fs::{self, File};
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

struct ProcessGroup(Child);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // The child leads a dedicated group. Kill descendants even when magi
        // has already exited, and reap magi on timeout or assertion failure.
        unsafe {
            libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

#[test]
fn slow_release_auth_does_not_delay_cli_exit() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for dir in [
        "bin",
        "cfg/magi",
        "home/Library/Caches",
        "cache",
        "magi-home",
    ] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    fs::write(
        root.join("cfg/magi/config.toml"),
        "[update]\nmode = \"notify\"\ninterval = \"1s\"\n",
    )
    .unwrap();
    let gh = root.join("bin/gh");
    fs::write(
        &gh,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"$GH_CALL_LOG\"\nsleep 20\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let calls = root.join("gh-calls");
    let mut paths = vec![root.join("bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let output_path = root.join("output");
    let output = File::create(&output_path).unwrap();
    let start = Instant::now();
    let mut process = ProcessGroup(
        common::magi_cli(&root.join("magi-home"))
            .args(["task", "list"])
            .current_dir(root)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("MAGI_CONFIG_DIR", root.join("cfg"))
            // dirs uses HOME/Library/Caches on macOS and XDG_CACHE_HOME on Linux.
            .env("HOME", root.join("home"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("GH_CALL_LOG", &calls)
            // A synchronous auth lookup can occupy the sole runtime worker,
            // including the worker responsible for driving Tokio timers.
            .env("TOKIO_WORKER_THREADS", "1")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            // This test alone exercises updates, through fake gh and a private cache.
            .env_remove("MAGI_NO_AUTOUPDATE")
            .env_remove("MAGI_RUN")
            .env_remove("MAGI_NODE")
            .stdin(Stdio::null())
            // Descendants inherit output handles; measuring pipe EOF would
            // measure their lifetime rather than magi's exit.
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let limit = Duration::from_secs(8);
    let status = loop {
        let elapsed = start.elapsed();
        assert!(
            elapsed < limit,
            "magi did not exit within {limit:?}: {}",
            fs::read_to_string(&output_path).unwrap()
        );
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        status.success(),
        "magi failed: {}",
        fs::read_to_string(output_path).unwrap()
    );
    assert_eq!(fs::read_to_string(calls).unwrap(), "auth token\n");
}

#[test]
fn pinned_worker_helpers_never_replace_their_executable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("home/Library/Caches")).unwrap();
    fs::create_dir_all(root.join("cache")).unwrap();
    fs::create_dir_all(root.join("cfg/magi")).unwrap();
    fs::write(
        root.join("cfg/magi/config.toml"),
        "[update]\nmode = \"install\"\ninterval = \"1s\"\n",
    )
    .unwrap();
    let gh = root.join("bin/gh");
    fs::write(
        &gh,
        "#!/bin/sh\nprintf called > \"$GH_CALL_LOG\"\nsleep 20\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![root.join("bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    for args in [vec!["task", "list"], vec!["self-update", "--yes"]] {
        let output_path = root.join("output");
        let output = File::create(&output_path).unwrap();
        let mut process = ProcessGroup(
            common::magi_cli(&root.join("magi-home"))
                .args(&args)
                .current_dir(root)
                .env("PATH", std::env::join_paths(&paths).unwrap())
                .env("MAGI_CONFIG_DIR", root.join("cfg"))
                .env("MAGI_WORKER_GENERATION", "pinned-test-generation")
                .env("GH_CALL_LOG", root.join("gh-calls"))
                .env("HOME", root.join("home"))
                .env("XDG_CACHE_HOME", root.join("cache"))
                .env_remove("GH_TOKEN")
                .env_remove("GITHUB_TOKEN")
                .env_remove("MAGI_NO_AUTOUPDATE")
                .stdin(Stdio::null())
                .stdout(output.try_clone().unwrap())
                .stderr(output)
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let started = Instant::now();
        let status = loop {
            assert!(
                started.elapsed() < Duration::from_secs(8),
                "pinned helper attempted release work"
            );
            if let Some(status) = process.0.try_wait().unwrap() {
                break status;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let output = fs::read_to_string(&output_path).unwrap();
        if args[0] == "self-update" {
            assert!(!status.success());
        } else {
            assert!(status.success(), "{output}");
        }
        assert!(
            !root.join("gh-calls").exists(),
            "pinned helper contacted the forge"
        );
    }
}
